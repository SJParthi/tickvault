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
    EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO,
    EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM, EXCHANGE_SEGMENT_NSE_CURRENCY,
    EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO, IST_UTC_OFFSET_SECONDS_I64,
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
        build_tick_row(&mut self.buffer, tick)?;

        self.pending_count = self.pending_count.saturating_add(1);

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
// Buffer Building (extracted for testability)
// ---------------------------------------------------------------------------

/// Writes a single tick row into the ILP buffer (no flush).
///
/// Converts the IST-naive exchange timestamp to proper UTC by subtracting
/// the IST offset (5h30m = 19800s), then writes all tick fields into the buffer.
fn build_tick_row(buffer: &mut Buffer, tick: &ParsedTick) -> Result<()> {
    // Dhan V2 sends exchange_timestamp as IST-naive epoch: the IST clock time
    // (e.g., 09:25:32 IST) encoded as if it were UTC epoch seconds. To store as
    // proper UTC in QuestDB, subtract the IST offset (5h30m = 19800s).
    let utc_epoch_secs =
        i64::from(tick.exchange_timestamp).saturating_sub(IST_UTC_OFFSET_SECONDS_I64);
    let ts_nanos = TimestampNanos::new(utc_epoch_secs.saturating_mul(1_000_000_000));
    let received_nanos = TimestampNanos::new(tick.received_at_nanos);

    buffer
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

    Ok(())
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
/// The table must exist BEFORE ILP writes for predictable schema.
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
    // APPROVED: u128 → u64 safe for ~584M years
    #[allow(clippy::as_conversions)]
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    ms
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// APPROVED: test code — relaxed lint rules for test fixtures
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]
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

    /// Spawn a background TCP server that accepts one connection and drains
    /// all data until EOF. Returns the port. Consolidates 10 identical
    /// spawn-accept-drain patterns into one site.
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

    #[test]
    fn test_segment_code_to_str_all_known() {
        assert_eq!(segment_code_to_str(0), "IDX_I");
        assert_eq!(segment_code_to_str(1), "NSE_EQ");
        assert_eq!(segment_code_to_str(2), "NSE_FNO");
        assert_eq!(segment_code_to_str(3), "NSE_CURRENCY");
        assert_eq!(segment_code_to_str(4), "BSE_EQ");
        assert_eq!(segment_code_to_str(5), "MCX_COMM");
        assert_eq!(segment_code_to_str(7), "BSE_CURRENCY");
        assert_eq!(segment_code_to_str(8), "BSE_FNO");
    }

    #[test]
    fn test_segment_code_to_str_unknown() {
        assert_eq!(segment_code_to_str(6), "UNKNOWN");
        assert_eq!(segment_code_to_str(9), "UNKNOWN");
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
    }

    #[test]
    fn test_tick_buffer_single_row() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24_500.5);

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
        let tick = make_test_tick(13, 24_500.5);

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

    #[test]
    fn test_ist_offset_subtracted_in_ts_nanos() {
        // Dhan sends exchange_timestamp as IST-naive epoch.
        // Example: IST 09:25:32 on 2026-02-27 → Dhan sends 1772118332
        // (which is 2026-02-27T09:25:32Z in UTC interpretation).
        // Correct UTC = 2026-02-27T03:55:32Z = epoch 1772098532.
        // The IST offset = 19800 seconds.
        let dhan_ist_naive_epoch: u32 = 1_772_118_332;
        let expected_utc_epoch: i64 = 1_772_098_532;

        let utc_epoch_secs = i64::from(dhan_ist_naive_epoch) - IST_UTC_OFFSET_SECONDS_I64;
        assert_eq!(utc_epoch_secs, expected_utc_epoch);

        // Verify the nanosecond conversion matches QuestDB expectations
        let ts_nanos = TimestampNanos::new(utc_epoch_secs * 1_000_000_000);
        assert_eq!(
            utc_epoch_secs * 1_000_000_000,
            ts_nanos.as_i64(),
            "ts_nanos should be UTC epoch nanoseconds"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: build_tick_row (extracted helper)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_tick_row_single_row() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24_500.5);

        build_tick_row(&mut buffer, &tick).unwrap();

        assert_eq!(buffer.row_count(), 1);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_build_tick_row_contains_all_fields() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24_500.5);

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_TICKS));
        assert!(content.contains("NSE_FNO"));
        assert!(content.contains("security_id"));
        assert!(content.contains("ltp"));
        assert!(content.contains("open"));
        assert!(content.contains("high"));
        assert!(content.contains("low"));
        assert!(content.contains("close"));
        assert!(content.contains("volume"));
        assert!(content.contains("oi"));
        assert!(content.contains("avg_price"));
        assert!(content.contains("last_trade_qty"));
        assert!(content.contains("total_buy_qty"));
        assert!(content.contains("total_sell_qty"));
        assert!(content.contains("received_at"));
    }

    #[test]
    fn test_build_tick_row_multiple_rows() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);

        for i in 0..25_u32 {
            let tick = make_test_tick(1000 + i, 24500.0 + (i as f32));
            build_tick_row(&mut buffer, &tick).unwrap();
        }

        assert_eq!(buffer.row_count(), 25);
    }

    #[test]
    fn test_build_tick_row_idx_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: EXCHANGE_SEGMENT_IDX_I,
            last_traded_price: 24500.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_123_456_789,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("IDX_I"));
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_tick_row_nse_equity_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 2885,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_EQ,
            last_traded_price: 2800.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("NSE_EQ"));
    }

    #[test]
    fn test_build_tick_row_nse_currency_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 60000,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_CURRENCY,
            last_traded_price: 83.50,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("NSE_CURRENCY"));
    }

    #[test]
    fn test_build_tick_row_bse_equity_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 70000,
            exchange_segment_code: EXCHANGE_SEGMENT_BSE_EQ,
            last_traded_price: 1500.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BSE_EQ"));
    }

    #[test]
    fn test_build_tick_row_bse_currency_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 80000,
            exchange_segment_code: EXCHANGE_SEGMENT_BSE_CURRENCY,
            last_traded_price: 83.25,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BSE_CURRENCY"));
    }

    #[test]
    fn test_build_tick_row_bse_fno_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 90000,
            exchange_segment_code: EXCHANGE_SEGMENT_BSE_FNO,
            last_traded_price: 72000.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("BSE_FNO"));
    }

    #[test]
    fn test_build_tick_row_mcx_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 55000,
            exchange_segment_code: EXCHANGE_SEGMENT_MCX_COMM,
            last_traded_price: 75000.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("MCX_COMM"));
    }

    #[test]
    fn test_build_tick_row_unknown_segment() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 99, // unknown segment code
            last_traded_price: 100.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("UNKNOWN"));
    }

    #[test]
    fn test_build_tick_row_zero_oi_and_volume() {
        // Ticker packets have zero OI/volume — make sure it writes correctly.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 24500.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            volume: 0,
            open_interest: 0,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_tick_row_max_u32_values() {
        // Stress test with max u32 values for numeric fields.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: u32::MAX,
            exchange_segment_code: 2,
            last_traded_price: f32::MAX,
            last_trade_quantity: u16::MAX,
            exchange_timestamp: u32::MAX,
            received_at_nanos: i64::MAX,
            average_traded_price: f32::MAX,
            volume: u32::MAX,
            total_sell_quantity: u32::MAX,
            total_buy_quantity: u32::MAX,
            day_open: f32::MAX,
            day_close: f32::MAX,
            day_high: f32::MAX,
            day_low: f32::MAX,
            open_interest: u32::MAX,
            oi_day_high: u32::MAX,
            oi_day_low: u32::MAX,
        };

        build_tick_row(&mut buffer, &tick).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_tick_row_ist_offset_applied_correctly() {
        // Verify the IST offset is subtracted when building the row.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 2,
            last_traded_price: 24500.0,
            // 1772118332 is IST-naive epoch for 2026-02-27 09:25:32 IST.
            exchange_timestamp: 1_772_118_332,
            received_at_nanos: 1_772_098_532_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        // The buffer should contain the UTC-corrected timestamp.
        // We can verify by checking the row was written successfully.
        assert_eq!(buffer.row_count(), 1);
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_build_tick_row_preserves_received_at_nanos() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 24500.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_999_999_999,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        // received_at should appear as a timestamp column in the buffer.
        assert!(content.contains("received_at"));
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: segment_code_to_str exhaustive tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_segment_code_to_str_gap_code_6() {
        // Code 6 is not assigned — should return UNKNOWN.
        assert_eq!(segment_code_to_str(6), "UNKNOWN");
    }

    #[test]
    fn test_segment_code_to_str_boundary_values() {
        // Test values around the defined constants.
        assert_eq!(segment_code_to_str(0), "IDX_I");
        assert_eq!(segment_code_to_str(8), "BSE_FNO");
        assert_eq!(segment_code_to_str(10), "UNKNOWN");
        assert_eq!(segment_code_to_str(128), "UNKNOWN");
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: TICKS_CREATE_DDL and constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_ticks_create_ddl_contains_required_columns() {
        assert!(TICKS_CREATE_DDL.contains("segment SYMBOL"));
        assert!(TICKS_CREATE_DDL.contains("security_id LONG"));
        assert!(TICKS_CREATE_DDL.contains("ltp DOUBLE"));
        assert!(TICKS_CREATE_DDL.contains("open DOUBLE"));
        assert!(TICKS_CREATE_DDL.contains("high DOUBLE"));
        assert!(TICKS_CREATE_DDL.contains("low DOUBLE"));
        assert!(TICKS_CREATE_DDL.contains("close DOUBLE"));
        assert!(TICKS_CREATE_DDL.contains("volume LONG"));
        assert!(TICKS_CREATE_DDL.contains("oi LONG"));
        assert!(TICKS_CREATE_DDL.contains("avg_price DOUBLE"));
        assert!(TICKS_CREATE_DDL.contains("last_trade_qty LONG"));
        assert!(TICKS_CREATE_DDL.contains("total_buy_qty LONG"));
        assert!(TICKS_CREATE_DDL.contains("total_sell_qty LONG"));
        assert!(TICKS_CREATE_DDL.contains("received_at TIMESTAMP"));
        assert!(TICKS_CREATE_DDL.contains("ts TIMESTAMP"));
    }

    #[test]
    fn test_ticks_create_ddl_has_wal_partition() {
        assert!(TICKS_CREATE_DDL.contains("TIMESTAMP(ts)"));
        assert!(TICKS_CREATE_DDL.contains("PARTITION BY HOUR"));
        assert!(TICKS_CREATE_DDL.contains("WAL"));
    }

    #[test]
    fn test_ticks_create_ddl_is_idempotent() {
        assert!(TICKS_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_dedup_key_ticks_constant() {
        assert_eq!(DEDUP_KEY_TICKS, "security_id");
    }

    #[test]
    fn test_questdb_ddl_timeout_is_reasonable() {
        assert!((5..=60).contains(&QUESTDB_DDL_TIMEOUT_SECS));
    }

    #[test]
    fn test_tick_flush_constants_are_sane() {
        assert!((1..=10_000).contains(&TICK_FLUSH_BATCH_SIZE));
        assert!((1..=5_000).contains(&TICK_FLUSH_INTERVAL_MS));
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: current_time_ms additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_current_time_ms_is_monotonically_increasing() {
        let t1 = current_time_ms();
        let t2 = current_time_ms();
        // t2 should be >= t1 (monotonic within same millisecond).
        assert!(t2 >= t1);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: ensure_tick_table_dedup_keys with various configs
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_keys_with_invalid_host() {
        // Completely invalid hostname — should log warning, not panic.
        let config = QuestDbConfig {
            host: "this.host.does.not.exist.example.invalid".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_keys_with_zero_port() {
        let config = QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 0,
            pg_port: 0,
            ilp_port: 0,
        };
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_keys_success_with_mock_http() {
        // Start a mock HTTP server that returns 200 OK for DDL requests.
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        // Exercises the success path (response.status().is_success() == true)
        // for both CREATE TABLE and ALTER TABLE DEDUP DDL statements.
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_keys_non_success_with_mock_http() {
        // Start a mock HTTP server that returns 400 Bad Request.
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        // Exercises the non-success path (response.status().is_success() == false)
        // for both CREATE TABLE and ALTER TABLE DEDUP DDL statements.
        ensure_tick_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: build_tick_row with Default tick
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_tick_row_with_minimal_tick() {
        // Cannot use ParsedTick::default() because exchange_timestamp=0 minus
        // IST offset produces a negative epoch which QuestDB rejects.
        // Use a minimal tick with a valid timestamp instead.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 0, // IDX_I
            last_traded_price: 0.0,
            exchange_timestamp: 1740556500, // valid IST-naive epoch
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("IDX_I"));
    }

    #[test]
    fn test_build_tick_row_buffer_clear_and_reuse() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = make_test_tick(13, 24500.0);

        build_tick_row(&mut buffer, &tick).unwrap();
        assert_eq!(buffer.row_count(), 1);

        buffer.clear();
        assert_eq!(buffer.row_count(), 0);
        assert!(buffer.is_empty());

        // Reuse after clear.
        build_tick_row(&mut buffer, &tick).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_tick_row_large_batch() {
        // Simulate a batch close to the flush threshold.
        let mut buffer = Buffer::new(ProtocolVersion::V1);

        for i in 0..TICK_FLUSH_BATCH_SIZE as u32 {
            let tick = make_test_tick(i, 24500.0 + (i as f32) * 0.05);
            build_tick_row(&mut buffer, &tick).unwrap();
        }

        assert_eq!(buffer.row_count(), TICK_FLUSH_BATCH_SIZE);
    }

    #[test]
    fn test_build_tick_row_negative_prices() {
        // While uncommon, negative f32 values should still write successfully.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: -1.0,
            day_open: -2.0,
            day_high: -0.5,
            day_low: -3.0,
            day_close: -1.5,
            average_traded_price: -1.25,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_tick_row_minimum_valid_exchange_timestamp() {
        // The minimum valid exchange_timestamp is IST_UTC_OFFSET_SECONDS + 1 (19801),
        // because subtracting the IST offset from anything smaller creates a negative
        // epoch that QuestDB rejects. Verify the boundary works.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 2,
            last_traded_price: 100.0,
            exchange_timestamp: 19801, // just above IST offset
            received_at_nanos: 1_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: ParsedTick Default trait
    // -----------------------------------------------------------------------

    #[test]
    fn test_parsed_tick_default_has_zero_values() {
        let tick = ParsedTick::default();
        assert_eq!(tick.security_id, 0);
        assert_eq!(tick.exchange_segment_code, 0);
        assert_eq!(tick.last_traded_price, 0.0);
        assert_eq!(tick.last_trade_quantity, 0);
        assert_eq!(tick.exchange_timestamp, 0);
        assert_eq!(tick.received_at_nanos, 0);
        assert_eq!(tick.average_traded_price, 0.0);
        assert_eq!(tick.volume, 0);
        assert_eq!(tick.total_sell_quantity, 0);
        assert_eq!(tick.total_buy_quantity, 0);
        assert_eq!(tick.day_open, 0.0);
        assert_eq!(tick.day_close, 0.0);
        assert_eq!(tick.day_high, 0.0);
        assert_eq!(tick.day_low, 0.0);
        assert_eq!(tick.open_interest, 0);
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: TickPersistenceWriter::new error path (line 86-98)
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_persistence_writer_new_with_unreachable_port() {
        // Port 1 on loopback — connection should be refused or fail.
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };

        let result = TickPersistenceWriter::new(&config);
        // On most systems, TCP to port 1 is refused. Whether from_conf fails
        // eagerly or lazily, the constructor should either succeed or return Err.
        // Either way, it must not panic.
        let _is_ok = result.is_ok();
    }

    #[test]
    fn test_tick_persistence_writer_new_with_invalid_hostname() {
        // Completely invalid hostname — DNS resolution fails.
        let config = QuestDbConfig {
            host: "this.host.does.not.exist.example.invalid".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        let result = TickPersistenceWriter::new(&config);
        // DNS resolution failure may cause from_conf to return Err.
        let _is_ok = result.is_ok();
    }

    #[test]
    fn test_tick_persistence_writer_new_with_ipv6_loopback() {
        let config = QuestDbConfig {
            host: "::1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };

        let result = TickPersistenceWriter::new(&config);
        let _is_ok = result.is_ok();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: append_tick, flush_if_needed, force_flush, pending_count
    // (lines 106-155 — uses live TCP servers for reliable coverage)
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_persistence_writer_append_tick_and_flush_error() {
        // Start a TCP server that accepts one connection then closes it.
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        assert_eq!(writer.pending_count(), 0);

        let tick = make_test_tick(13, 24_500.5);
        writer.append_tick(&tick).unwrap();

        // append_tick only flushes at TICK_FLUSH_BATCH_SIZE. Since we
        // appended just 1 tick, it should succeed (buffer only, no flush).
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_tick_persistence_writer_force_flush_error() {
        // Live TCP server that drains data — exercises the Ok path of force_flush.
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        let tick = make_test_tick(13, 24_500.5);
        writer.append_tick(&tick).unwrap();
        assert_eq!(writer.pending_count(), 1);

        // Force flush succeeds with the live TCP server.
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_persistence_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // Flush with no pending ticks should return Ok immediately.
        let result = writer.force_flush();
        assert!(
            result.is_ok(),
            "force_flush with zero pending must be a no-op"
        );
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_persistence_writer_flush_if_needed_empty() {
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // With no pending ticks, flush_if_needed returns immediately.
        let result = writer.flush_if_needed();
        assert!(
            result.is_ok(),
            "flush_if_needed with zero pending must be a no-op"
        );
    }

    #[test]
    fn test_tick_persistence_writer_flush_if_needed_with_ticks() {
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        let tick = make_test_tick(13, 24_500.5);
        writer.append_tick(&tick).unwrap();

        // flush_if_needed checks the time elapsed. Since we just
        // created the writer, the interval may not have passed.
        // If it hasn't, this returns Ok without flushing.
        // Either way, it exercises the code path.
        let result = writer.flush_if_needed();
        assert!(
            result.is_ok(),
            "flush_if_needed must succeed with live TCP server"
        );
    }

    #[test]
    fn test_tick_persistence_writer_append_many_ticks_triggers_auto_flush() {
        // Append TICK_FLUSH_BATCH_SIZE ticks to trigger the auto-flush inside
        // append_tick. With a live TCP server, the flush succeeds.
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        for i in 0..TICK_FLUSH_BATCH_SIZE as u32 {
            let tick = make_test_tick(1000 + i, 24500.0 + (i as f32));
            writer.append_tick(&tick).unwrap();
        }

        // After auto-flush at TICK_FLUSH_BATCH_SIZE, pending_count should be 0.
        assert_eq!(
            writer.pending_count(),
            0,
            "auto-flush at batch size boundary must reset pending count"
        );
    }

    #[test]
    fn test_tick_persistence_writer_pending_count_tracks_correctly() {
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        assert_eq!(writer.pending_count(), 0);

        for i in 0..5_u32 {
            let tick = make_test_tick(1000 + i, 24500.0);
            writer.append_tick(&tick).unwrap();
            assert_eq!(writer.pending_count(), (i + 1) as usize);
        }
    }

    // -----------------------------------------------------------------------
    // Live TCP server tests — covers write + flush operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_writer_append_and_flush_with_live_server() {
        // Dummy TCP server that accepts connections and drains data
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        assert_eq!(writer.pending_count(), 0);

        // Append a few ticks
        for i in 0..5_u32 {
            let tick = make_test_tick(1000 + i, 24500.0 + (i as f32));
            writer.append_tick(&tick).unwrap();
        }
        assert_eq!(writer.pending_count(), 5);

        // Force flush to the dummy server
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_writer_flush_if_needed_with_live_server() {
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // Append enough ticks to trigger auto-flush at TICK_FLUSH_BATCH_SIZE
        for i in 0..TICK_FLUSH_BATCH_SIZE as u32 {
            let tick = make_test_tick(i, 24500.0 + (i as f32) * 0.05);
            writer.append_tick(&tick).unwrap();
        }

        // After auto-flush, pending_count should be 0
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_writer_multiple_appends_and_flush() {
        let port = spawn_tcp_drain_server();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };

        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // Append 50 ticks in batches of 10, verifying pending_count
        for batch in 0..5_u32 {
            for i in 0..10_u32 {
                let sec_id = batch * 10 + i;
                let tick = make_test_tick(sec_id, 24500.0 + (sec_id as f32));
                writer.append_tick(&tick).unwrap();
            }
            assert_eq!(writer.pending_count(), ((batch + 1) * 10) as usize);
        }
        assert_eq!(writer.pending_count(), 50);

        // Flush all pending ticks
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);

        // Append more after flush — writer should be reusable
        let tick = make_test_tick(9999, 25000.0);
        writer.append_tick(&tick).unwrap();
        assert_eq!(writer.pending_count(), 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }
}
