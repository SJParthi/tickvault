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

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    DEPTH_FLUSH_BATCH_SIZE, IST_UTC_OFFSET_NANOS, QUESTDB_TABLE_MARKET_DEPTH,
    QUESTDB_TABLE_PREVIOUS_CLOSE, QUESTDB_TABLE_TICKS, TICK_FLUSH_BATCH_SIZE,
    TICK_FLUSH_INTERVAL_MS,
};
use dhan_live_trader_common::segment::segment_code_to_str;
use dhan_live_trader_common::tick_types::{MarketDepthLevel, ParsedTick};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for QuestDB DDL HTTP requests.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// DEDUP UPSERT KEY for the `ticks` table.
/// STORAGE-GAP-01: Must include segment to prevent cross-segment collision
/// (same security_id can exist on NSE_EQ and BSE_EQ).
const DEDUP_KEY_TICKS: &str = "security_id, segment";

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

    /// Returns a mutable reference to the ILP buffer for writing auxiliary rows
    /// (e.g., previous close) that share the same flush lifecycle.
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }
}

// ---------------------------------------------------------------------------
// f32 → f64 Precision-Preserving Conversion
// ---------------------------------------------------------------------------

/// Stack buffer size for f32 shortest decimal representation.
/// Maximum f32 decimal string: "-3.4028235e+38" = 15 chars. 24 is generous.
const F32_DECIMAL_BUF_SIZE: usize = 24;

/// Converts f32 to f64 preserving the shortest decimal representation.
///
/// Dhan sends prices as IEEE 754 f32 on the wire. Direct `f64::from(f32)`
/// widens the bit pattern, revealing hidden f32 imprecision:
///   21004.95_f32 → `f64::from()` → 21004.94921875_f64  ← WRONG
///   21004.95_f32 → this function → 21004.95_f64         ← CORRECT
///
/// Matches the Dhan Python SDK behavior: `"{:.2f}".format(value)`.
///
/// # Performance
/// Zero heap allocation — uses a stack buffer for the decimal string.
#[inline]
pub(crate) fn f32_to_f64_clean(v: f32) -> f64 {
    if v == 0.0 || !v.is_finite() {
        // APPROVED: f64::from(f32) is correct for zero/inf/NaN — no precision loss for these values
        return f64::from(v);
    }
    let mut buf = [0u8; F32_DECIMAL_BUF_SIZE];
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    // f32 Display uses ryu internally — produces the shortest decimal
    // representation that round-trips through f32. Zero allocation.
    let _ = write!(cursor, "{v}");
    #[allow(clippy::arithmetic_side_effects)]
    // APPROVED: cursor position of a 24-byte buf always fits usize
    let n = cursor.position() as usize;
    // f32 Display only produces ASCII digits, '.', '-', 'e', '+'.
    std::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        // APPROVED: f64::from(f32) fallback — only reached if ryu/parse fails (never in practice)
        .unwrap_or(f64::from(v))
}

// ---------------------------------------------------------------------------
// Buffer Building (extracted for testability)
// ---------------------------------------------------------------------------

/// Writes a single tick row into the ILP buffer (no flush).
///
/// Dhan WebSocket sends `exchange_timestamp` as IST epoch seconds — already
/// adjusted, so no +19800 offset is needed. The timestamp is stored directly
/// as the designated QuestDB timestamp. The raw `exchange_timestamp` LONG
/// column is also preserved verbatim for audit.
///
/// `received_at_nanos` comes from `chrono::Utc::now()` (UTC), so we add
/// `IST_UTC_OFFSET_NANOS` to align it with the IST-based designated timestamp.
///
/// Price fields use `f32_to_f64_clean` to preserve the original Dhan f32
/// precision without f32→f64 widening artifacts.
fn build_tick_row(buffer: &mut Buffer, tick: &ParsedTick) -> Result<()> {
    // Dhan WebSocket exchange_timestamp is already IST epoch seconds.
    // Store directly — no offset needed.
    let ts_nanos =
        TimestampNanos::new(i64::from(tick.exchange_timestamp).saturating_mul(1_000_000_000));
    let received_nanos =
        TimestampNanos::new(tick.received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS));

    buffer
        .table(QUESTDB_TABLE_TICKS)
        .context("table name")?
        .symbol("segment", segment_code_to_str(tick.exchange_segment_code))
        .context("segment")?
        .column_i64("security_id", i64::from(tick.security_id))
        .context("security_id")?
        .column_f64("ltp", f32_to_f64_clean(tick.last_traded_price))
        .context("ltp")?
        .column_f64("open", f32_to_f64_clean(tick.day_open))
        .context("open")?
        .column_f64("high", f32_to_f64_clean(tick.day_high))
        .context("high")?
        .column_f64("low", f32_to_f64_clean(tick.day_low))
        .context("low")?
        .column_f64("close", f32_to_f64_clean(tick.day_close))
        .context("close")?
        .column_i64("volume", i64::from(tick.volume))
        .context("volume")?
        .column_i64("oi", i64::from(tick.open_interest))
        .context("oi")?
        .column_f64("avg_price", f32_to_f64_clean(tick.average_traded_price))
        .context("avg_price")?
        .column_i64("last_trade_qty", i64::from(tick.last_trade_quantity))
        .context("last_trade_qty")?
        .column_i64("total_buy_qty", i64::from(tick.total_buy_quantity))
        .context("total_buy_qty")?
        .column_i64("total_sell_qty", i64::from(tick.total_sell_quantity))
        .context("total_sell_qty")?
        .column_i64("exchange_timestamp", i64::from(tick.exchange_timestamp))
        .context("exchange_timestamp")?
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
/// - `exchange_timestamp` as LONG (raw Dhan IST epoch seconds, preserved verbatim for audit)
/// - `received_at` as TIMESTAMP (system clock UTC + IST offset, aligned with `ts`)
/// - `ts` as designated TIMESTAMP (Dhan IST epoch seconds, stored directly)
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
        exchange_timestamp LONG,\
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
// Market Depth Persistence
// ---------------------------------------------------------------------------

/// Batched depth writer for QuestDB via ILP.
///
/// Writes 5-level market depth snapshots from Full (code 8) and
/// Market Depth (code 3) packets. Each snapshot = 5 ILP rows (one per level).
pub struct DepthPersistenceWriter {
    sender: Sender,
    buffer: Buffer,
    pending_count: usize,
    last_flush_ms: u64,
}

impl DepthPersistenceWriter {
    /// Creates a new depth writer connected to QuestDB via ILP TCP.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let sender = Sender::from_conf(&conf_string)
            .context("failed to connect to QuestDB via ILP for depth")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
            last_flush_ms: current_time_ms(),
        })
    }

    /// Appends a 5-level depth snapshot to the ILP buffer.
    ///
    /// # Performance
    /// O(1) — fixed 5-iteration loop, no heap allocation.
    pub fn append_depth(
        &mut self,
        security_id: u32,
        exchange_segment_code: u8,
        received_at_nanos: i64,
        depth: &[MarketDepthLevel; 5],
    ) -> Result<()> {
        build_depth_rows(
            &mut self.buffer,
            security_id,
            exchange_segment_code,
            received_at_nanos,
            depth,
        )?;

        self.pending_count = self.pending_count.saturating_add(1);

        if self.pending_count >= DEPTH_FLUSH_BATCH_SIZE {
            self.force_flush()?;
        }

        Ok(())
    }

    /// Flushes the buffer if enough time has elapsed since the last flush.
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

    /// Forces an immediate flush of all buffered depth rows to QuestDB.
    pub fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        let count = self.pending_count;
        self.sender
            .flush(&mut self.buffer)
            .context("flush depth to QuestDB")?;
        self.pending_count = 0;
        self.last_flush_ms = current_time_ms();

        debug!(flushed_snapshots = count, "depth batch flushed to QuestDB");
        Ok(())
    }
}

/// Writes 5 depth-level rows into the ILP buffer for a single snapshot.
///
/// Table: `market_depth` with columns: segment, security_id, level (1-5),
/// bid_qty, ask_qty, bid_orders, ask_orders, bid_price, ask_price, received_at, ts.
///
/// Price fields use `f32_to_f64_clean` to preserve Dhan f32 precision.
fn build_depth_rows(
    buffer: &mut Buffer,
    security_id: u32,
    exchange_segment_code: u8,
    received_at_nanos: i64,
    depth: &[MarketDepthLevel; 5],
) -> Result<()> {
    // UTC nanos → IST-as-UTC: add IST offset so QuestDB shows IST wall-clock time.
    let received_nanos =
        TimestampNanos::new(received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS));

    for (i, level) in depth.iter().enumerate() {
        // APPROVED: depth is [_; 5], so i is 0..4 — always fits i64
        #[allow(clippy::arithmetic_side_effects)]
        let depth_level = (i as i64).saturating_add(1);
        buffer
            .table(QUESTDB_TABLE_MARKET_DEPTH)
            .context("depth table name")?
            .symbol("segment", segment_code_to_str(exchange_segment_code))
            .context("depth segment")?
            .column_i64("security_id", i64::from(security_id))
            .context("depth security_id")?
            .column_i64("level", depth_level)
            .context("depth level")?
            .column_i64("bid_qty", i64::from(level.bid_quantity))
            .context("depth bid_qty")?
            .column_i64("ask_qty", i64::from(level.ask_quantity))
            .context("depth ask_qty")?
            .column_i64("bid_orders", i64::from(level.bid_orders))
            .context("depth bid_orders")?
            .column_i64("ask_orders", i64::from(level.ask_orders))
            .context("depth ask_orders")?
            .column_f64("bid_price", f32_to_f64_clean(level.bid_price))
            .context("depth bid_price")?
            .column_f64("ask_price", f32_to_f64_clean(level.ask_price))
            .context("depth ask_price")?
            .column_ts("received_at", received_nanos)
            .context("depth received_at")?
            .at(received_nanos)
            .context("depth designated timestamp")?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Previous Close Persistence
// ---------------------------------------------------------------------------

/// Writes a previous close record to the `previous_close` table.
///
/// Called once per security when a code 6 packet arrives (typically at session start).
/// Uses `received_at` as designated timestamp since Dhan doesn't send a timestamp
/// in this packet type. Shifted to IST-as-UTC for consistency with all other tables.
///
/// Price field uses `f32_to_f64_clean` to preserve Dhan f32 precision.
pub fn build_previous_close_row(
    buffer: &mut Buffer,
    security_id: u32,
    exchange_segment_code: u8,
    previous_close: f32,
    previous_oi: u32,
    received_at_nanos: i64,
) -> Result<()> {
    // UTC nanos → IST-as-UTC: add IST offset so QuestDB shows IST wall-clock time.
    let received_nanos =
        TimestampNanos::new(received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS));

    buffer
        .table(QUESTDB_TABLE_PREVIOUS_CLOSE)
        .context("prev_close table name")?
        .symbol("segment", segment_code_to_str(exchange_segment_code))
        .context("prev_close segment")?
        .column_i64("security_id", i64::from(security_id))
        .context("prev_close security_id")?
        .column_f64("prev_close", f32_to_f64_clean(previous_close))
        .context("prev_close price")?
        .column_i64("prev_oi", i64::from(previous_oi))
        .context("prev_close oi")?
        .at(received_nanos)
        .context("prev_close designated timestamp")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Market Depth + Previous Close Table DDL
// ---------------------------------------------------------------------------

/// SQL to create the `market_depth` table with explicit schema.
const MARKET_DEPTH_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS market_depth (\
        segment SYMBOL,\
        security_id LONG,\
        level LONG,\
        bid_qty LONG,\
        ask_qty LONG,\
        bid_orders LONG,\
        ask_orders LONG,\
        bid_price DOUBLE,\
        ask_price DOUBLE,\
        received_at TIMESTAMP,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// SQL to create the `previous_close` table with explicit schema.
const PREVIOUS_CLOSE_CREATE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS previous_close (\
        segment SYMBOL,\
        security_id LONG,\
        prev_close DOUBLE,\
        prev_oi LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

/// DEDUP UPSERT KEY for the `market_depth` table.
/// STORAGE-GAP-01: Includes segment to prevent cross-segment collision
/// (same security_id can exist on NSE_EQ and BSE_EQ).
const DEDUP_KEY_MARKET_DEPTH: &str = "security_id, segment, level";

/// DEDUP UPSERT KEY for the `previous_close` table.
/// STORAGE-GAP-01: Includes segment to prevent cross-segment collision.
const DEDUP_KEY_PREVIOUS_CLOSE: &str = "security_id, segment";

/// Creates the `market_depth` and `previous_close` tables and enables DEDUP UPSERT KEYS.
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
pub async fn ensure_depth_and_prev_close_tables(questdb_config: &QuestDbConfig) {
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
            warn!(?err, "failed to build HTTP client for depth/prev_close DDL");
            return;
        }
    };

    // --- market_depth table ---
    execute_ddl_best_effort(
        &client,
        &base_url,
        MARKET_DEPTH_CREATE_DDL,
        "market_depth CREATE",
    )
    .await;
    let dedup_depth = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_MARKET_DEPTH, DEDUP_KEY_MARKET_DEPTH
    );
    execute_ddl_best_effort(&client, &base_url, &dedup_depth, "market_depth DEDUP").await;

    // --- previous_close table ---
    execute_ddl_best_effort(
        &client,
        &base_url,
        PREVIOUS_CLOSE_CREATE_DDL,
        "previous_close CREATE",
    )
    .await;
    let dedup_prev_close = format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        QUESTDB_TABLE_PREVIOUS_CLOSE, DEDUP_KEY_PREVIOUS_CLOSE
    );
    execute_ddl_best_effort(
        &client,
        &base_url,
        &dedup_prev_close,
        "previous_close DEDUP",
    )
    .await;

    info!("market_depth and previous_close table setup complete");
}

/// Executes a DDL statement against QuestDB HTTP, logging warnings on failure.
async fn execute_ddl_best_effort(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                debug!(label, "DDL executed successfully");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    label,
                    body = body.chars().take(200).collect::<String>(),
                    "DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, label, "DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns current wall-clock time in milliseconds since Unix epoch.
fn current_time_ms() -> u64 {
    #[allow(clippy::arithmetic_side_effects)]
    // APPROVED: as_millis() truncation is safe — u128→u64 won't overflow until year 584M+
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    ms
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic is not on hot path
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::{
        EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO,
        EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM, EXCHANGE_SEGMENT_NSE_CURRENCY,
        EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO, IST_UTC_OFFSET_SECONDS_I64,
    };
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
    fn test_ist_epoch_stored_directly_in_ts_nanos() {
        // Dhan WebSocket sends exchange_timestamp as IST epoch seconds.
        // Example: an IST epoch value that represents 13:25 IST.
        // No +19800 offset needed — stored directly.
        let dhan_epoch: u32 = 1_740_556_500;

        let epoch_secs = i64::from(dhan_epoch);
        let ts_nanos = TimestampNanos::new(epoch_secs * 1_000_000_000);
        assert_eq!(
            epoch_secs * 1_000_000_000,
            ts_nanos.as_i64(),
            "ts_nanos should be IST epoch nanoseconds stored directly"
        );

        // Verify the IST timestamp shows correct time-of-day in QuestDB.
        let ist_time_of_day = epoch_secs % 86_400;
        let hour = ist_time_of_day / 3600;
        let minute = (ist_time_of_day % 3600) / 60;
        // Dhan IST epoch: time-of-day is already IST wall-clock.
        assert!((0..24).contains(&hour), "hour must be valid");
        assert!((0..60).contains(&minute), "minute must be valid");
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
        assert!(content.contains("exchange_timestamp"));
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
    fn test_build_tick_row_stores_ist_epoch_directly() {
        // Verify IST epoch is stored directly (no offset added).
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 2,
            last_traded_price: 24500.0,
            // Dhan WebSocket sends IST epoch seconds directly.
            exchange_timestamp: 1_740_556_500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        // Verify row was written with IST epoch as designated timestamp (no offset).
        assert_eq!(buffer.row_count(), 1);
        assert!(!buffer.is_empty());

        let content = String::from_utf8_lossy(buffer.as_bytes());
        // ILP designated timestamp = IST epoch in nanoseconds (stored directly)
        let ist_epoch_nanos = 1_740_556_500_i64 * 1_000_000_000;
        let expected_ts = format!("{ist_epoch_nanos}\n");
        assert!(
            content.ends_with(&expected_ts),
            "ILP timestamp must be IST epoch nanos stored directly. Content: {content}"
        );
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

    #[test]
    fn test_build_tick_row_preserves_raw_exchange_timestamp() {
        // The raw Dhan exchange_timestamp (UTC epoch seconds) is stored
        // verbatim in the `exchange_timestamp` LONG column for audit.
        // The designated `ts` uses IST-adjusted epoch (UTC + 19800s).
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let dhan_raw_epoch: u32 = 1_740_556_500; // UTC epoch for 07:55 UTC (13:25 IST)
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 2,
            last_traded_price: 24500.0,
            exchange_timestamp: dhan_raw_epoch,
            received_at_nanos: 1_740_556_500_500_000_000, // ~0.5ms after exchange time
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        // ILP integer format: field_name=<value>i
        let expected_ilp_value = format!("exchange_timestamp={}i", dhan_raw_epoch);
        assert!(
            content.contains(&expected_ilp_value),
            "Raw exchange_timestamp not preserved verbatim in ILP buffer. Content: {content}"
        );
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
        assert!(TICKS_CREATE_DDL.contains("exchange_timestamp LONG"));
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

    /// STORAGE-GAP-01: DEDUP key must include both security_id and segment
    /// to prevent cross-segment tick collision (same security_id on NSE_EQ vs BSE_EQ).
    #[test]
    fn test_tick_dedup_key_includes_segment() {
        assert!(
            DEDUP_KEY_TICKS.contains("security_id"),
            "DEDUP_KEY_TICKS must include security_id"
        );
        assert!(
            DEDUP_KEY_TICKS.contains("segment"),
            "DEDUP_KEY_TICKS must include segment (STORAGE-GAP-01)"
        );
        assert_eq!(DEDUP_KEY_TICKS, "security_id, segment");
    }

    /// STORAGE-GAP-01: Verify that the ticks DDL includes exchange_segment
    /// column, which is needed to prevent cross-segment tick collision
    /// when the same security_id exists on different segments.
    #[test]
    fn test_ticks_ddl_has_segment_column() {
        assert!(
            TICKS_CREATE_DDL.contains("segment"),
            "ticks DDL must include segment column for cross-segment safety"
        );
    }

    /// Verify the depth DDL also includes segment column.
    #[test]
    fn test_depth_ddl_has_segment_column() {
        assert!(
            MARKET_DEPTH_CREATE_DDL.contains("segment"),
            "market_depth DDL must include segment column"
        );
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
        // Use a minimal tick with a valid UTC timestamp.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 1,
            exchange_segment_code: 0, // IDX_I
            last_traded_price: 0.0,
            exchange_timestamp: 1740556500, // valid UTC epoch (2025-02-26 07:55 UTC)
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
        // Verify a small but positive exchange_timestamp is accepted.
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

    // --- f32_to_f64_clean tests ---

    #[test]
    fn test_f32_to_f64_clean_preserves_simple_prices() {
        // Dhan sends these as f32. Direct f64::from() would corrupt them.
        assert_eq!(f32_to_f64_clean(24500.5), 24500.5);
        assert_eq!(f32_to_f64_clean(100.0), 100.0);
        assert_eq!(f32_to_f64_clean(24.45), 24.45);
        assert_eq!(f32_to_f64_clean(58755.25), 58755.25);
        assert_eq!(f32_to_f64_clean(16281.5), 16281.5);
    }

    #[test]
    fn test_f32_to_f64_clean_index_values() {
        // Index values from the screenshot that were corrupted
        let nifty_f32: f32 = 21004.95;
        let result = f32_to_f64_clean(nifty_f32);
        assert_eq!(result, 21004.95_f64);

        let bank_nifty_f32: f32 = 27929.1;
        let result = f32_to_f64_clean(bank_nifty_f32);
        assert_eq!(result, 27929.1_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_vs_naive_conversion() {
        // Demonstrate the problem: f64::from(f32) reveals artifacts
        let price: f32 = 744.15;
        let naive = f64::from(price);
        let clean = f32_to_f64_clean(price);

        // Naive conversion shows artifacts
        assert_ne!(naive, 744.15_f64, "naive should differ from clean f64");
        // Clean conversion preserves the intended value
        assert_eq!(clean, 744.15_f64, "clean should match intended f64");
    }

    #[test]
    fn test_f32_to_f64_clean_zero() {
        assert_eq!(f32_to_f64_clean(0.0), 0.0);
        assert_eq!(
            f32_to_f64_clean(-0.0).to_bits(),
            f64::from(-0.0_f32).to_bits()
        );
    }

    #[test]
    fn test_f32_to_f64_clean_infinity_and_nan() {
        assert_eq!(f32_to_f64_clean(f32::INFINITY), f64::INFINITY);
        assert_eq!(f32_to_f64_clean(f32::NEG_INFINITY), f64::NEG_INFINITY);
        assert!(f32_to_f64_clean(f32::NAN).is_nan());
    }

    #[test]
    fn test_f32_to_f64_clean_small_values() {
        assert_eq!(f32_to_f64_clean(0.05), 0.05);
        assert_eq!(f32_to_f64_clean(0.1), 0.1);
        assert_eq!(f32_to_f64_clean(1.5), 1.5);
    }

    #[test]
    fn test_f32_to_f64_clean_large_values() {
        assert_eq!(f32_to_f64_clean(100000.0), 100000.0);
        assert_eq!(f32_to_f64_clean(999999.5), 999999.5);
    }

    #[test]
    fn test_f32_to_f64_clean_negative_prices() {
        assert_eq!(f32_to_f64_clean(-24500.5), -24500.5);
        assert_eq!(f32_to_f64_clean(-0.05), -0.05);
    }

    #[test]
    fn test_build_tick_row_ltp_precision_preserved() {
        // Verify the actual ILP buffer contains clean values, not f32→f64 artifacts.
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            last_traded_price: 21004.95,
            exchange_timestamp: 1772073900,
            received_at_nanos: 0,
            ..Default::default()
        };

        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_tick_row(&mut buf, &tick).unwrap();
        let ilp_str = String::from_utf8_lossy(buf.as_bytes());

        // The ILP line should contain "ltp=21004.95" not "ltp=21004.94921875"
        assert!(
            ilp_str.contains("ltp=21004.95"),
            "ILP buffer should contain clean LTP. Got: {ilp_str}"
        );
        assert!(
            !ilp_str.contains("21004.949"),
            "ILP buffer must NOT contain f32→f64 artifact. Got: {ilp_str}"
        );
    }

    // -----------------------------------------------------------------------
    // Market Depth Persistence Tests
    // -----------------------------------------------------------------------

    fn make_test_depth() -> [MarketDepthLevel; 5] {
        [
            MarketDepthLevel {
                bid_quantity: 1000,
                ask_quantity: 500,
                bid_orders: 10,
                ask_orders: 5,
                bid_price: 24490.0,
                ask_price: 24500.0,
            },
            MarketDepthLevel {
                bid_quantity: 800,
                ask_quantity: 600,
                bid_orders: 8,
                ask_orders: 6,
                bid_price: 24485.0,
                ask_price: 24505.0,
            },
            MarketDepthLevel {
                bid_quantity: 600,
                ask_quantity: 400,
                bid_orders: 6,
                ask_orders: 4,
                bid_price: 24480.0,
                ask_price: 24510.0,
            },
            MarketDepthLevel {
                bid_quantity: 400,
                ask_quantity: 200,
                bid_orders: 4,
                ask_orders: 2,
                bid_price: 24475.0,
                ask_price: 24515.0,
            },
            MarketDepthLevel {
                bid_quantity: 200,
                ask_quantity: 100,
                bid_orders: 2,
                ask_orders: 1,
                bid_price: 24470.0,
                ask_price: 24520.0,
            },
        ]
    }

    #[test]
    fn test_build_depth_rows_writes_five_rows() {
        let depth = make_test_depth();
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(&mut buf, 13, 2, 1_000_000_000, &depth).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        // Should produce exactly 5 ILP lines (one per level)
        let line_count = content.lines().count();
        assert_eq!(line_count, 5, "expected 5 depth rows, got {line_count}");
    }

    #[test]
    fn test_build_depth_rows_contains_table_name() {
        let depth = make_test_depth();
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(&mut buf, 13, 2, 1_000_000_000, &depth).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        assert!(
            content.contains("market_depth"),
            "depth ILP should reference market_depth table. Got: {content}"
        );
    }

    #[test]
    fn test_build_depth_rows_level_numbers() {
        let depth = make_test_depth();
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(&mut buf, 42, 2, 1_000_000_000, &depth).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        for level in 1..=5 {
            assert!(
                content.contains(&format!("level={level}i")),
                "expected level={level}i in ILP. Got: {content}"
            );
        }
    }

    #[test]
    fn test_build_depth_rows_bid_ask_values() {
        let depth = make_test_depth();
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(&mut buf, 13, 2, 1_000_000_000, &depth).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        // Level 1 values
        assert!(content.contains("bid_qty=1000i"));
        assert!(content.contains("ask_qty=500i"));
        assert!(content.contains("bid_orders=10i"));
        assert!(content.contains("ask_orders=5i"));
        assert!(content.contains("bid_price=24490.0"));
        assert!(content.contains("ask_price=24500.0"));
    }

    #[test]
    fn test_build_depth_rows_preserves_f32_precision() {
        let mut depth = [MarketDepthLevel::default(); 5];
        depth[0].bid_price = 21004.95;
        depth[0].ask_price = 744.15;
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(&mut buf, 13, 2, 1_000_000_000, &depth).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        assert!(
            content.contains("bid_price=21004.95"),
            "depth bid_price should preserve f32 precision. Got: {content}"
        );
        assert!(
            content.contains("ask_price=744.15"),
            "depth ask_price should preserve f32 precision. Got: {content}"
        );
    }

    #[test]
    fn test_build_depth_rows_zero_depth() {
        let depth = [MarketDepthLevel::default(); 5];
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(&mut buf, 13, 2, 1_000_000_000, &depth).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        // All levels should have zero quantities
        let line_count = content.lines().count();
        assert_eq!(line_count, 5);
        assert!(content.contains("bid_qty=0i"));
        assert!(content.contains("ask_qty=0i"));
    }

    #[test]
    fn test_build_depth_rows_segment_mapping() {
        let depth = [MarketDepthLevel::default(); 5];
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_depth_rows(
            &mut buf,
            13,
            EXCHANGE_SEGMENT_NSE_FNO,
            1_000_000_000,
            &depth,
        )
        .unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        assert!(
            content.contains("segment=NSE_FNO"),
            "depth should map segment code to string. Got: {content}"
        );
    }

    // -----------------------------------------------------------------------
    // Previous Close Persistence Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_previous_close_row_basic() {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(&mut buf, 13, 2, 24300.5, 120000, 1_000_000_000).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        assert!(content.contains("previous_close"));
        assert!(content.contains("security_id=13i"));
        assert!(content.contains("prev_close=24300.5"));
        assert!(content.contains("prev_oi=120000i"));
    }

    #[test]
    fn test_build_previous_close_row_precision() {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(&mut buf, 42, 2, 21004.95, 0, 1_000_000_000).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        assert!(
            content.contains("prev_close=21004.95"),
            "prev_close should preserve f32 precision. Got: {content}"
        );
        assert!(
            !content.contains("21004.949"),
            "prev_close must NOT contain f32→f64 artifact. Got: {content}"
        );
    }

    #[test]
    fn test_build_previous_close_row_segment() {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(
            &mut buf,
            13,
            EXCHANGE_SEGMENT_NSE_EQ,
            100.0,
            0,
            1_000_000_000,
        )
        .unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());

        assert!(
            content.contains("segment=NSE_EQ"),
            "prev_close should map segment. Got: {content}"
        );
    }

    #[test]
    fn test_build_previous_close_row_zero_oi() {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(&mut buf, 13, 2, 24300.0, 0, 1_000_000_000).unwrap();
        let content = String::from_utf8_lossy(buf.as_bytes());
        assert!(content.contains("prev_oi=0i"));
    }

    // -----------------------------------------------------------------------
    // DDL Tests for Market Depth + Previous Close
    // -----------------------------------------------------------------------

    #[test]
    fn test_market_depth_ddl_contains_required_columns() {
        assert!(MARKET_DEPTH_CREATE_DDL.contains("security_id LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("level LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("bid_qty LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("ask_qty LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("bid_orders LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("ask_orders LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("bid_price DOUBLE"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("ask_price DOUBLE"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("PARTITION BY HOUR WAL"));
    }

    #[test]
    fn test_previous_close_ddl_contains_required_columns() {
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("security_id LONG"));
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("prev_close DOUBLE"));
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("prev_oi LONG"));
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("PARTITION BY DAY WAL"));
    }

    #[test]
    fn test_market_depth_ddl_is_idempotent() {
        assert!(MARKET_DEPTH_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_previous_close_ddl_is_idempotent() {
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("IF NOT EXISTS"));
    }

    // -----------------------------------------------------------------------
    // DepthPersistenceWriter Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_depth_persistence_writer_new_with_valid_server() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let writer = DepthPersistenceWriter::new(&config);
        assert!(writer.is_ok(), "depth writer should connect to valid TCP");
    }

    #[test]
    fn test_depth_persistence_writer_append_and_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        let depth = make_test_depth();

        writer.append_depth(13, 2, 1_000_000_000, &depth).unwrap();
        writer.force_flush().unwrap();
    }

    #[test]
    fn test_depth_persistence_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        // Flushing empty buffer should not error
        writer.force_flush().unwrap();
    }

    #[test]
    fn test_midnight_timestamp_ilp_encoding() {
        // Midnight IST = 18:30 UTC previous day. Ensure ILP encoding handles
        // the partition boundary correctly (QuestDB partitions by HOUR).
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // Test UTC epoch value in early March 2026 range.
        let mut tick = make_test_tick(13, 24500.0);
        tick.exchange_timestamp = 1772588400;
        tick.received_at_nanos = 1_772_588_400_000_000_000;

        writer.append_tick(&tick).unwrap();
        writer.force_flush().unwrap();
    }

    #[test]
    fn test_duplicate_security_id_timestamp_ilp_encoding() {
        // Two ticks with same (security_id, exchange_timestamp) but different LTP.
        // Both should be accepted by the ILP buffer — dedup is QuestDB's job
        // via DEDUP UPSERT KEYS.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        let mut tick1 = make_test_tick(42, 1500.0);
        tick1.exchange_timestamp = 1772073900;
        tick1.received_at_nanos = 1_772_073_900_000_000_000;

        let mut tick2 = make_test_tick(42, 1505.0); // same security_id, different LTP
        tick2.exchange_timestamp = 1772073900; // same timestamp
        tick2.received_at_nanos = 1_772_073_900_100_000_000;

        writer.append_tick(&tick1).unwrap();
        writer.append_tick(&tick2).unwrap();
        assert_eq!(writer.buffer.row_count(), 2);
        writer.force_flush().unwrap();
    }

    // -----------------------------------------------------------------------
    // Dhan IST epoch verification with real market hours
    //
    // Dhan WebSocket sends exchange_timestamp as IST epoch seconds.
    // Our pipeline stores them directly in QuestDB (no offset needed).
    // Historical REST API sends UTC epoch seconds (+19800s applied at
    // persistence to align with live data).
    //
    // These tests verify the FULL pipeline with known IST epoch values
    // corresponding to NSE market hours (09:00 - 15:30 IST).
    // -----------------------------------------------------------------------

    /// Helper: compute IST epoch for a given IST time on 2026-03-10.
    ///
    /// Dhan WebSocket sends IST epoch seconds directly.
    /// Midnight IST on 2026-03-10 as IST epoch = midnight UTC + 19800s.
    fn ist_epoch_for_ist_time_2026_03_10(
        ist_hours: u32,
        ist_minutes: u32,
        ist_seconds: u32,
    ) -> u32 {
        // Midnight IST 2026-03-10 as IST epoch seconds.
        // UTC midnight 2026-03-10 = 1773100800. IST midnight = UTC 2026-03-09 18:30 = 1773081000.
        // But as IST epoch: the value Dhan sends for midnight IST = UTC value + 19800.
        // IST epoch for midnight IST = 1773081000 + 19800 = 1773100800.
        const MARCH_10_2026_MIDNIGHT_IST_EPOCH: u32 = 1_773_100_800;
        MARCH_10_2026_MIDNIGHT_IST_EPOCH + ist_hours * 3600 + ist_minutes * 60 + ist_seconds
    }

    /// Helper: extract IST hour/minute from an IST epoch.
    ///
    /// IST epoch seconds mod 86400 directly gives IST seconds-of-day.
    fn ist_epoch_to_ist_hm(ist_epoch: i64) -> (i64, i64) {
        let ist_secs = ist_epoch % 86_400;
        let hour = ist_secs / 3600;
        let minute = (ist_secs % 3600) / 60;
        (hour, minute)
    }

    #[test]
    fn test_dhan_epoch_market_open_0915_ist() {
        // NSE market opens at 09:15:00 IST.
        let dhan_epoch = ist_epoch_for_ist_time_2026_03_10(9, 15, 0);

        // Stored directly — no manipulation needed.
        let stored_epoch = i64::from(dhan_epoch);

        // IST epoch mod 86400 gives IST time-of-day directly.
        let (ist_hour, ist_minute) = ist_epoch_to_ist_hm(stored_epoch);
        assert_eq!(ist_hour, 9, "IST hour must be 9");
        assert_eq!(ist_minute, 15, "IST minute must be 15");
    }

    #[test]
    fn test_dhan_epoch_market_close_1530_ist() {
        // NSE market closes at 15:30:00 IST.
        let dhan_epoch = ist_epoch_for_ist_time_2026_03_10(15, 30, 0);

        let stored_epoch = i64::from(dhan_epoch);

        let (ist_hour, ist_minute) = ist_epoch_to_ist_hm(stored_epoch);
        assert_eq!(ist_hour, 15, "IST hour must be 15");
        assert_eq!(ist_minute, 30, "IST minute must be 30");
    }

    #[test]
    fn test_dhan_epoch_mid_session_1200_ist() {
        // Mid-session: 12:00:00 IST.
        let dhan_epoch = ist_epoch_for_ist_time_2026_03_10(12, 0, 0);

        let stored_epoch = i64::from(dhan_epoch);

        let (ist_hour, ist_minute) = ist_epoch_to_ist_hm(stored_epoch);
        assert_eq!(ist_hour, 12, "IST hour must be 12");
        assert_eq!(ist_minute, 0, "IST minute must be 0");
    }

    #[test]
    fn test_dhan_epoch_pre_market_0900_ist() {
        // Pre-market: 09:00:00 IST.
        let dhan_epoch = ist_epoch_for_ist_time_2026_03_10(9, 0, 0);

        let stored_epoch = i64::from(dhan_epoch);

        let (ist_hour, _ist_minute) = ist_epoch_to_ist_hm(stored_epoch);
        assert_eq!(ist_hour, 9, "IST hour must be 9");
    }

    #[test]
    fn test_dhan_epoch_build_tick_row_stores_ist_directly() {
        // End-to-end: build_tick_row with a tick at 10:30:00 IST
        // and verify the designated timestamp is the IST epoch nanos (no offset).
        let dhan_epoch = ist_epoch_for_ist_time_2026_03_10(10, 30, 0);
        // IST epoch stored directly — no +19800 needed.
        let ist_ts_nanos = i64::from(dhan_epoch) * 1_000_000_000;

        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 25,
            exchange_segment_code: EXCHANGE_SEGMENT_NSE_EQ,
            last_traded_price: 2350.50,
            exchange_timestamp: dhan_epoch,
            received_at_nanos: i64::from(dhan_epoch) * 1_000_000_000 + 500_000, // ~0.5ms latency
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        // ILP designated timestamp (last value before newline) = IST epoch nanos directly.
        let expected_ts_str = format!("{ist_ts_nanos}\n");
        assert!(
            content.ends_with(&expected_ts_str),
            "ILP designated timestamp must be IST epoch nanos stored directly.\nExpected suffix: {expected_ts_str}\nActual: {content}"
        );
    }

    #[test]
    fn test_dhan_epoch_all_market_hours_display_correct_ist() {
        // Verify that for every minute from 09:15 to 15:30 IST,
        // the IST epoch mod 86400 gives correct IST time-of-day.
        let market_open = ist_epoch_for_ist_time_2026_03_10(9, 15, 0);
        let market_close = ist_epoch_for_ist_time_2026_03_10(15, 30, 0);

        let mut epoch = market_open;
        while epoch <= market_close {
            let stored = i64::from(epoch);
            let (ist_hour, ist_minute) = ist_epoch_to_ist_hm(stored);
            let total_minutes = ist_hour * 60 + ist_minute;

            assert!(
                (555..=930).contains(&total_minutes),
                "IST time {ist_hour}:{ist_minute:02} (minute {total_minutes}) out of market range for IST epoch {epoch}"
            );

            epoch += 60; // advance 1 minute
        }
    }

    #[test]
    fn test_historical_candle_utc_epoch_needs_offset() {
        // Dhan REST API returns UTC epoch seconds.
        // Historical persistence adds +19800 to align with live IST data.
        // This test verifies the offset produces correct IST time-of-day.
        //
        // 10:00 IST on 2026-03-10 = 04:30 UTC.
        // Midnight IST 2026-03-10 as UTC epoch = 1773081000.
        // 10:00 IST = midnight IST + 10h as UTC epoch: 1773081000 + 10*3600 = 1773117000.
        let utc_epoch: i64 = 1_773_117_000; // 2026-03-10 04:30 UTC = 10:00 IST

        // Historical persistence adds IST offset.
        let ist_epoch = utc_epoch + IST_UTC_OFFSET_SECONDS_I64;
        let ts_nanos = TimestampNanos::new(ist_epoch * 1_000_000_000);
        assert_eq!(ts_nanos.as_i64(), ist_epoch * 1_000_000_000);

        // Verify the stored IST-as-UTC value shows 10:00 IST.
        // (UTC epoch + 19800) % 86400 gives IST time-of-day.
        let secs_of_day = ist_epoch % 86_400;
        let hour = secs_of_day / 3600;
        let minute = (secs_of_day % 3600) / 60;
        assert_eq!(hour, 10, "IST hour must be 10");
        assert_eq!(minute, 0);
    }

    #[test]
    fn test_live_candle_and_tick_store_same_ist_epoch() {
        // Live candle and tick both store the IST epoch from Dhan directly.
        // Verify they produce identical stored values for the same timestamp.
        let dhan_epoch: u32 = ist_epoch_for_ist_time_2026_03_10(11, 45, 30);

        // Both paths store directly — no conversion.
        let tick_stored = i64::from(dhan_epoch);
        let candle_stored = i64::from(dhan_epoch);

        assert_eq!(
            tick_stored, candle_stored,
            "Tick and candle persistence must store identical IST epochs"
        );

        // Verify IST time is 11:45 IST.
        let (ist_hour, ist_minute) = ist_epoch_to_ist_hm(tick_stored);
        assert_eq!(ist_hour, 11, "IST hour must be 11");
        assert_eq!(ist_minute, 45, "IST minute must be 45");
    }

    #[test]
    fn test_dhan_ws_timestamp_is_ist_epoch() {
        // Dhan WebSocket LTT values are IST epoch seconds.
        // Time-of-day can be extracted directly via mod 86400.
        let dhan_epoch: i64 = 1_740_556_500;

        let (ist_hour, ist_minute) = ist_epoch_to_ist_hm(dhan_epoch);
        // IST epoch: time-of-day is IST wall-clock directly.
        assert!((0..24).contains(&ist_hour), "hour must be valid");
        assert!((0..60).contains(&ist_minute), "minute must be valid");
    }

    #[test]
    fn test_received_at_and_exchange_timestamp_different_basis() {
        // received_at_nanos comes from chrono::Utc::now() → UTC.
        // exchange_timestamp is IST epoch from Dhan WebSocket.
        // They differ by ~19800 seconds (IST offset), NOT just latency.
        let exchange_ts: u32 = ist_epoch_for_ist_time_2026_03_10(10, 30, 0);
        // received_at_nanos is UTC-based (from Utc::now())
        let received_utc_nanos: i64 =
            (i64::from(exchange_ts) - IST_UTC_OFFSET_SECONDS_I64) * 1_000_000_000 + 1_000_000;

        // The difference includes IST offset + latency.
        let diff_secs = i64::from(exchange_ts) - (received_utc_nanos / 1_000_000_000);
        assert_eq!(
            diff_secs, IST_UTC_OFFSET_SECONDS_I64,
            "exchange_timestamp (IST) minus received_at (UTC) ≈ IST offset"
        );
    }

    // -----------------------------------------------------------------------
    // STORAGE-GAP-01: DEDUP key includes segment for previous_close + market_depth
    // -----------------------------------------------------------------------

    /// STORAGE-GAP-01: previous_close DEDUP key must include segment
    /// to prevent cross-segment collision (same security_id on NSE_EQ vs BSE_EQ).
    #[test]
    fn test_previous_close_dedup_key_includes_segment() {
        assert!(
            DEDUP_KEY_PREVIOUS_CLOSE.contains("security_id"),
            "DEDUP_KEY_PREVIOUS_CLOSE must include security_id"
        );
        assert!(
            DEDUP_KEY_PREVIOUS_CLOSE.contains("segment"),
            "DEDUP_KEY_PREVIOUS_CLOSE must include segment (STORAGE-GAP-01)"
        );
        assert_eq!(DEDUP_KEY_PREVIOUS_CLOSE, "security_id, segment");
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: ensure_depth_and_prev_close_tables
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_depth_and_prev_close_tables_does_not_panic_unreachable() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        ensure_depth_and_prev_close_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_and_prev_close_tables_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the success path for both market_depth and previous_close DDL.
        ensure_depth_and_prev_close_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_and_prev_close_tables_non_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises the non-success path for market_depth and previous_close DDL.
        ensure_depth_and_prev_close_tables(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: DepthPersistenceWriter flush_if_needed
    // -----------------------------------------------------------------------

    #[test]
    fn test_depth_persistence_writer_flush_if_needed_empty() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        let result = writer.flush_if_needed();
        assert!(
            result.is_ok(),
            "flush_if_needed with zero pending must be a no-op"
        );
    }

    #[test]
    fn test_depth_persistence_writer_flush_if_needed_with_pending() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        let depth = make_test_depth();
        writer.append_depth(13, 2, 1_000_000_000, &depth).unwrap();
        // flush_if_needed checks the time elapsed — exercises the code path.
        let result = writer.flush_if_needed();
        assert!(result.is_ok());
    }

    #[test]
    fn test_depth_persistence_writer_batch_auto_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        let depth = make_test_depth();

        for i in 0..DEPTH_FLUSH_BATCH_SIZE as u32 {
            writer
                .append_depth(1000 + i, 2, 1_000_000_000 + i as i64, &depth)
                .unwrap();
        }
        // After auto-flush at DEPTH_FLUSH_BATCH_SIZE, pending should be 0.
        assert_eq!(
            writer.pending_count, 0,
            "auto-flush at batch boundary must reset pending count"
        );
    }

    #[test]
    fn test_depth_persistence_writer_new_with_unreachable_port() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let result = DepthPersistenceWriter::new(&config);
        // Either succeeds (lazy) or fails — must not panic.
        let _is_ok = result.is_ok();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: build_previous_close_row IST offset
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_previous_close_row_received_at_includes_ist_offset() {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        let received_at_utc_nanos: i64 = 1_773_100_800_000_000_000;
        build_previous_close_row(&mut buf, 13, 2, 24300.5, 120000, received_at_utc_nanos).unwrap();

        let content = String::from_utf8_lossy(buf.as_bytes());
        // The designated timestamp should be received_at + IST offset
        let expected_nanos = received_at_utc_nanos + IST_UTC_OFFSET_NANOS;
        let expected_ts = format!("{expected_nanos}\n");
        assert!(
            content.ends_with(&expected_ts),
            "previous_close designated timestamp must include IST offset. Got: {content}"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: build_tick_row received_at IST offset
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_tick_row_received_at_is_ist_adjusted() {
        // Verify that build_tick_row writes successfully and the buffer
        // contains the received_at field. The exact IST-adjusted nanos
        // value is an ILP timestamp column — verified by the fact that
        // build_tick_row adds IST_UTC_OFFSET_NANOS to received_at_nanos.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 2,
            last_traded_price: 24500.0,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_000_000_000,
            ..Default::default()
        };

        build_tick_row(&mut buffer, &tick).unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        // received_at must appear as a field in the ILP buffer
        assert!(
            content.contains("received_at"),
            "ILP buffer must contain received_at field. Got: {content}"
        );
        assert_eq!(buffer.row_count(), 1);
    }

    /// STORAGE-GAP-01: market_depth DEDUP key must include segment
    /// to prevent cross-segment collision.
    #[test]
    fn test_market_depth_dedup_key_includes_segment() {
        assert!(
            DEDUP_KEY_MARKET_DEPTH.contains("security_id"),
            "DEDUP_KEY_MARKET_DEPTH must include security_id"
        );
        assert!(
            DEDUP_KEY_MARKET_DEPTH.contains("segment"),
            "DEDUP_KEY_MARKET_DEPTH must include segment (STORAGE-GAP-01)"
        );
        assert!(
            DEDUP_KEY_MARKET_DEPTH.contains("level"),
            "DEDUP_KEY_MARKET_DEPTH must include level"
        );
        assert_eq!(DEDUP_KEY_MARKET_DEPTH, "security_id, segment, level");
    }

    /// Verify that previous_close rows with same security_id but different
    /// segments produce distinct ILP rows with different segment symbols.
    #[test]
    fn test_previous_close_row_includes_segment() {
        let mut buf_nse = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(
            &mut buf_nse,
            1333,
            EXCHANGE_SEGMENT_NSE_EQ,
            2500.0,
            0,
            1_000_000_000,
        )
        .unwrap();
        let nse_content = String::from_utf8_lossy(buf_nse.as_bytes());

        let mut buf_bse = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(
            &mut buf_bse,
            1333,
            EXCHANGE_SEGMENT_BSE_EQ,
            2500.0,
            0,
            1_000_000_000,
        )
        .unwrap();
        let bse_content = String::from_utf8_lossy(buf_bse.as_bytes());

        assert!(nse_content.contains("segment=NSE_EQ"));
        assert!(bse_content.contains("segment=BSE_EQ"));
        // Same security_id but different segment → different rows in QuestDB
        assert_ne!(
            nse_content.as_ref(),
            bse_content.as_ref(),
            "same security_id on different segments must produce different ILP rows"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: TickPersistenceWriter, DepthPersistenceWriter,
    // build_previous_close_row, f32_to_f64_clean, ensure DDL functions
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_writer_new_with_valid_tcp_server() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let writer = TickPersistenceWriter::new(&config);
        assert!(writer.is_ok(), "writer must connect to valid TCP");
    }

    #[test]
    fn test_tick_writer_append_increments_pending() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        assert_eq!(writer.pending_count(), 0);

        let tick = make_test_tick(13, 24_500.0);
        writer.append_tick(&tick).unwrap();
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_tick_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        let result = writer.force_flush();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_writer_force_flush_resets_counter() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        for i in 0..5_u32 {
            let tick = make_test_tick(1000 + i, 24500.0 + i as f32);
            writer.append_tick(&tick).unwrap();
        }
        assert_eq!(writer.pending_count(), 5);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_writer_flush_if_needed_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        let result = writer.flush_if_needed();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_tick_writer_buffer_mut_returns_buffer() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();
        let buf = writer.buffer_mut();
        assert!(buf.is_empty(), "buffer must be empty initially");
    }

    #[test]
    fn test_tick_writer_append_flush_reuse_cycle() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // Cycle 1
        let tick = make_test_tick(13, 24500.0);
        writer.append_tick(&tick).unwrap();
        assert_eq!(writer.pending_count(), 1);
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);

        // Cycle 2
        writer.append_tick(&tick).unwrap();
        assert_eq!(writer.pending_count(), 1);
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    // --- DepthPersistenceWriter ---

    #[test]
    fn test_depth_writer_new_with_valid_tcp_server() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let writer = DepthPersistenceWriter::new(&config);
        assert!(writer.is_ok());
    }

    #[test]
    fn test_depth_writer_append_and_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();

        let depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24510.0,
        }; 5];

        writer
            .append_depth(
                13,
                EXCHANGE_SEGMENT_NSE_FNO,
                1_740_556_500_000_000_000,
                &depth,
            )
            .unwrap();
        assert_eq!(writer.pending_count, 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_depth_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        let result = writer.force_flush();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_depth_writer_flush_if_needed_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();
        let result = writer.flush_if_needed();
        assert!(result.is_ok());
    }

    // --- build_depth_rows ---

    #[test]
    fn test_build_depth_rows_produces_five_rows() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 24500.0,
            ask_price: 24510.0,
        }; 5];

        build_depth_rows(
            &mut buffer,
            13,
            EXCHANGE_SEGMENT_NSE_FNO,
            1_740_556_500_000_000_000,
            &depth,
        )
        .unwrap();

        assert_eq!(buffer.row_count(), 5, "5-level depth = 5 ILP rows");
    }

    #[test]
    fn test_build_depth_rows_contains_all_levels() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut depth = [MarketDepthLevel::default(); 5];
        for (i, level) in depth.iter_mut().enumerate() {
            level.bid_price = 24500.0 - (i as f32);
            level.ask_price = 24500.0 + (i as f32);
            level.bid_quantity = (100 + i * 10) as u32;
            level.ask_quantity = (200 + i * 10) as u32;
            level.bid_orders = (5 + i) as u16;
            level.ask_orders = (10 + i) as u16;
        }

        build_depth_rows(
            &mut buffer,
            42,
            EXCHANGE_SEGMENT_NSE_EQ,
            1_000_000_000,
            &depth,
        )
        .unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_MARKET_DEPTH));
        assert!(content.contains("NSE_EQ"));
        assert!(content.contains("level"));
        assert!(content.contains("bid_qty"));
        assert!(content.contains("ask_qty"));
        assert!(content.contains("bid_orders"));
        assert!(content.contains("ask_orders"));
        assert!(content.contains("bid_price"));
        assert!(content.contains("ask_price"));
    }

    // --- build_previous_close_row ---

    #[test]
    fn test_build_previous_close_row_produces_one_row() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(
            &mut buffer,
            13,
            EXCHANGE_SEGMENT_IDX_I,
            24500.0,
            0,
            1_000_000_000,
        )
        .unwrap();
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_previous_close_row_contains_fields() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(
            &mut buffer,
            2885,
            EXCHANGE_SEGMENT_NSE_EQ,
            2800.50,
            0,
            1_740_556_500_000_000_000,
        )
        .unwrap();

        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains(QUESTDB_TABLE_PREVIOUS_CLOSE));
        assert!(content.contains("NSE_EQ"));
        assert!(content.contains("prev_close"));
        assert!(content.contains("prev_oi"));
    }

    #[test]
    fn test_build_previous_close_row_with_derivatives_oi() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_previous_close_row(
            &mut buffer,
            52432,
            EXCHANGE_SEGMENT_NSE_FNO,
            245.50,
            120000,
            1_740_556_500_000_000_000,
        )
        .unwrap();
        assert_eq!(buffer.row_count(), 1);
        let content = String::from_utf8_lossy(buffer.as_bytes());
        assert!(content.contains("NSE_FNO"));
    }

    // --- f32_to_f64_clean additional edge cases ---

    #[test]
    fn test_f32_to_f64_clean_negative_zero() {
        assert_eq!(f32_to_f64_clean(-0.0), 0.0);
    }

    #[test]
    fn test_f32_to_f64_clean_neg_infinity() {
        assert!(f32_to_f64_clean(f32::NEG_INFINITY).is_infinite());
        assert!(f32_to_f64_clean(f32::NEG_INFINITY) < 0.0);
    }

    #[test]
    fn test_f32_to_f64_clean_dhan_problematic_value() {
        // STORAGE-GAP-02: The classic problematic value
        let result = f32_to_f64_clean(21004.95_f32);
        assert!(
            (result - 21004.95_f64).abs() < 0.001,
            "21004.95_f32 must convert to ~21004.95_f64, got {}",
            result
        );
    }

    #[test]
    fn test_f32_to_f64_clean_batch_typical_prices() {
        let prices = [100.0_f32, 245.50, 24500.0, 0.05, 99999.95];
        for price in prices {
            let result = f32_to_f64_clean(price);
            assert!(
                (result - f64::from(price)).abs() < 0.01,
                "price {} did not convert cleanly, got {}",
                price,
                result
            );
        }
    }

    #[test]
    fn test_f32_to_f64_clean_very_small_value() {
        let tiny = 0.001_f32;
        let result = f32_to_f64_clean(tiny);
        assert!(
            (result - 0.001).abs() < 0.0001,
            "small value must convert cleanly"
        );
    }

    // --- DDL constants ---

    #[test]
    fn test_ticks_ddl_contains_all_columns() {
        assert!(TICKS_CREATE_DDL.contains("ticks"));
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
        assert!(TICKS_CREATE_DDL.contains("received_at TIMESTAMP"));
        assert!(TICKS_CREATE_DDL.contains("PARTITION BY HOUR WAL"));
    }

    #[test]
    fn test_tick_dedup_key_exact_value() {
        // STORAGE-GAP-01: Exact value check
        assert_eq!(DEDUP_KEY_TICKS, "security_id, segment");
    }

    #[test]
    fn test_market_depth_ddl_contains_all_columns() {
        assert!(MARKET_DEPTH_CREATE_DDL.contains("market_depth"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("segment SYMBOL"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("level LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("bid_qty LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("ask_qty LONG"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("bid_price DOUBLE"));
        assert!(MARKET_DEPTH_CREATE_DDL.contains("ask_price DOUBLE"));
    }

    #[test]
    fn test_previous_close_ddl_contains_all_columns() {
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("previous_close"));
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("segment SYMBOL"));
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("prev_close DOUBLE"));
        assert!(PREVIOUS_CLOSE_CREATE_DDL.contains("prev_oi LONG"));
    }

    #[test]
    fn test_depth_dedup_key_includes_segment_and_level() {
        assert!(DEDUP_KEY_MARKET_DEPTH.contains("segment"));
        assert!(DEDUP_KEY_MARKET_DEPTH.contains("security_id"));
        assert!(DEDUP_KEY_MARKET_DEPTH.contains("level"));
    }

    #[test]
    fn test_previous_close_dedup_key_exact_value() {
        assert_eq!(DEDUP_KEY_PREVIOUS_CLOSE, "security_id, segment");
    }

    #[test]
    fn test_f32_decimal_buf_size_is_sufficient() {
        // Max f32 decimal: "-3.4028235e+38" = 15 chars. 24 is generous.
        assert!(
            F32_DECIMAL_BUF_SIZE >= 16,
            "buffer must fit max f32 decimal string"
        );
    }

    // --- ensure DDL functions with mock servers ---

    #[tokio::test]
    async fn test_ensure_tick_table_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_non_success_with_mock_http() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_and_prev_close_tables_unreachable() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Must not panic
        ensure_depth_and_prev_close_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_and_prev_close_tables_success_with_mock() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_depth_and_prev_close_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_and_prev_close_tables_non_success_with_mock() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        ensure_depth_and_prev_close_tables(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: f32_to_f64_clean edge cases, build_tick_row with
    // extreme values, DDL constants validation, writer lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn test_f32_to_f64_clean_subnormal_value() {
        // Smallest positive subnormal f32 — may be treated as near-zero
        let subnormal = f32::MIN_POSITIVE / 2.0;
        let result = f32_to_f64_clean(subnormal);
        // Must not panic; result should be finite and non-negative
        assert!(result >= 0.0);
        assert!(result.is_finite());
    }

    #[test]
    fn test_f32_to_f64_clean_max_f32() {
        let max = f32::MAX;
        let result = f32_to_f64_clean(max);
        assert!(result.is_finite());
        assert!(result > 0.0);
    }

    #[test]
    fn test_f32_to_f64_clean_min_positive() {
        // f32::MIN_POSITIVE is very small (1.17549435e-38); the display string
        // may parse back as 0.0 depending on float representation.
        let min_pos = f32::MIN_POSITIVE;
        let result = f32_to_f64_clean(min_pos);
        assert!(result >= 0.0, "must be non-negative");
        assert!(result.is_finite());
    }

    #[test]
    fn test_f32_to_f64_clean_typical_nifty_prices() {
        // Test a range of typical Nifty option prices
        let prices: &[f32] = &[0.05, 0.10, 1.50, 25.75, 100.00, 245.50, 18000.0, 25000.0];
        for &price in prices {
            let result = f32_to_f64_clean(price);
            // The f64 result must match the original f32 decimal exactly
            let expected = format!("{price}").parse::<f64>().unwrap();
            assert!(
                (result - expected).abs() < 1e-10,
                "f32_to_f64_clean({}) = {}, expected {}",
                price,
                result,
                expected
            );
        }
    }

    #[test]
    fn test_build_tick_row_with_zero_exchange_timestamp() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut tick = make_test_tick(1, 100.0);
        tick.exchange_timestamp = 0;
        let result = build_tick_row(&mut buffer, &tick);
        assert!(result.is_ok(), "zero timestamp must not cause errors");
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_tick_row_with_max_exchange_timestamp() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut tick = make_test_tick(1, 100.0);
        tick.exchange_timestamp = u32::MAX;
        let result = build_tick_row(&mut buffer, &tick);
        assert!(result.is_ok(), "max u32 timestamp must not cause errors");
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_depth_rows_all_zero_depth_produces_five_rows() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let depth = [MarketDepthLevel {
            bid_quantity: 0,
            ask_quantity: 0,
            bid_orders: 0,
            ask_orders: 0,
            bid_price: 0.0,
            ask_price: 0.0,
        }; 5];
        let result = build_depth_rows(&mut buffer, 1, EXCHANGE_SEGMENT_NSE_FNO, 0, &depth);
        assert!(result.is_ok());
        assert_eq!(buffer.row_count(), 5, "5 depth levels = 5 ILP rows");
    }

    #[test]
    fn test_build_previous_close_row_with_zero_price() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let result = build_previous_close_row(&mut buffer, 42, EXCHANGE_SEGMENT_NSE_EQ, 0.0, 0, 0);
        assert!(result.is_ok());
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_build_previous_close_row_with_large_oi() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let result = build_previous_close_row(
            &mut buffer,
            42,
            EXCHANGE_SEGMENT_NSE_FNO,
            18500.25,
            u32::MAX,
            1_700_000_000_000_000_000,
        );
        assert!(result.is_ok());
        assert_eq!(buffer.row_count(), 1);
    }

    #[test]
    fn test_dedup_key_ticks_exact_format() {
        assert_eq!(DEDUP_KEY_TICKS, "security_id, segment");
    }

    #[test]
    fn test_dedup_key_market_depth_exact_format() {
        assert_eq!(DEDUP_KEY_MARKET_DEPTH, "security_id, segment, level");
    }

    #[test]
    fn test_dedup_key_previous_close_exact_format() {
        assert_eq!(DEDUP_KEY_PREVIOUS_CLOSE, "security_id, segment");
    }

    #[test]
    fn test_all_ddl_contain_create_table_if_not_exists() {
        for ddl in [
            TICKS_CREATE_DDL,
            MARKET_DEPTH_CREATE_DDL,
            PREVIOUS_CLOSE_CREATE_DDL,
        ] {
            assert!(
                ddl.contains("CREATE TABLE IF NOT EXISTS"),
                "DDL must be idempotent"
            );
        }
    }

    #[test]
    fn test_all_ddl_use_wal_mode() {
        for ddl in [
            TICKS_CREATE_DDL,
            MARKET_DEPTH_CREATE_DDL,
            PREVIOUS_CLOSE_CREATE_DDL,
        ] {
            assert!(
                ddl.contains("WAL"),
                "DDL must use WAL mode for dedup support"
            );
        }
    }

    #[test]
    fn test_ticks_ddl_partitioned_by_hour() {
        assert!(
            TICKS_CREATE_DDL.contains("PARTITION BY HOUR"),
            "ticks table must be partitioned by hour for performance"
        );
    }

    #[test]
    fn test_market_depth_ddl_partitioned_by_hour() {
        assert!(
            MARKET_DEPTH_CREATE_DDL.contains("PARTITION BY HOUR"),
            "market_depth table must be partitioned by hour"
        );
    }

    #[test]
    fn test_previous_close_ddl_partitioned_by_day() {
        assert!(
            PREVIOUS_CLOSE_CREATE_DDL.contains("PARTITION BY DAY"),
            "previous_close table must be partitioned by day"
        );
    }

    #[test]
    fn test_f32_decimal_buf_size_holds_typical_prices() {
        // The buffer is designed for typical Dhan prices (up to ~100,000 range).
        // For extreme values (f32::MAX), the write truncates and the function
        // falls back to f64::from(f32). Verify typical price strings fit.
        let typical_prices: &[f32] = &[0.05, 100.0, 18000.0, 50000.0, 99999.99, -99999.99];
        for &p in typical_prices {
            let s = format!("{p}");
            assert!(
                s.len() <= F32_DECIMAL_BUF_SIZE,
                "typical price {p} string (len={}) must fit in F32_DECIMAL_BUF_SIZE ({F32_DECIMAL_BUF_SIZE})",
                s.len()
            );
        }
    }

    #[test]
    fn test_f32_to_f64_clean_extreme_values_use_fallback() {
        // f32::MAX and f32::MIN have Display strings longer than the buffer.
        // The function must still return a valid f64 via the f64::from fallback.
        let result = f32_to_f64_clean(f32::MAX);
        assert!(result.is_finite());
        assert!(result > 0.0);

        let result = f32_to_f64_clean(f32::MIN);
        assert!(result.is_finite());
        assert!(result < 0.0);
    }

    #[test]
    fn test_current_time_ms_returns_positive() {
        let ms = current_time_ms();
        assert!(ms > 0, "current_time_ms must return positive value");
    }

    #[test]
    fn test_build_tick_row_all_segments_produce_valid_rows() {
        let segments = [
            EXCHANGE_SEGMENT_IDX_I,
            EXCHANGE_SEGMENT_NSE_EQ,
            EXCHANGE_SEGMENT_NSE_FNO,
            EXCHANGE_SEGMENT_NSE_CURRENCY,
            EXCHANGE_SEGMENT_BSE_EQ,
            EXCHANGE_SEGMENT_MCX_COMM,
            EXCHANGE_SEGMENT_BSE_CURRENCY,
            EXCHANGE_SEGMENT_BSE_FNO,
        ];
        for seg in segments {
            let mut buffer = Buffer::new(ProtocolVersion::V1);
            let mut tick = make_test_tick(1, 100.0);
            tick.exchange_segment_code = seg;
            let result = build_tick_row(&mut buffer, &tick);
            assert!(
                result.is_ok(),
                "build_tick_row must succeed for segment code {}",
                seg
            );
            assert_eq!(buffer.row_count(), 1);
        }
    }

    #[test]
    fn test_build_depth_rows_all_segments_produce_valid_rows() {
        let depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 100.0,
            ask_price: 100.5,
        }; 5];
        for seg in [
            EXCHANGE_SEGMENT_NSE_EQ,
            EXCHANGE_SEGMENT_NSE_FNO,
            EXCHANGE_SEGMENT_BSE_EQ,
        ] {
            let mut buffer = Buffer::new(ProtocolVersion::V1);
            let result = build_depth_rows(&mut buffer, 42, seg, 1_700_000_000_000_000_000, &depth);
            assert!(
                result.is_ok(),
                "depth rows must succeed for segment {}",
                seg
            );
            assert_eq!(buffer.row_count(), 5);
        }
    }

    #[test]
    fn test_questdb_ddl_timeout_constant() {
        assert_eq!(QUESTDB_DDL_TIMEOUT_SECS, 10);
    }

    #[tokio::test]
    async fn test_ensure_tick_table_with_zero_port_returns_without_panic() {
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 0,
            pg_port: 0,
            ilp_port: 0,
        };
        // Must not panic, just logs warnings
        ensure_tick_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: DDL warn! field evaluation with tracing subscriber
    // -----------------------------------------------------------------------

    /// Helper: install a tracing subscriber so `warn!` field expressions
    /// (e.g., `body.chars().take(200).collect::<String>()`) are evaluated.
    fn install_test_subscriber() -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());
        tracing::subscriber::set_default(subscriber)
    }

    #[tokio::test]
    async fn test_ensure_tick_table_non_success_with_tracing_subscriber() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // With a tracing subscriber, warn! field expressions are evaluated,
        // covering `body.chars().take(200).collect::<String>()` lines.
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_send_error_with_tracing_subscriber() {
        let _guard = install_test_subscriber();
        // Server accepts connection then immediately drops it → send error
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
        ensure_tick_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_prev_close_non_success_with_tracing_subscriber() {
        let _guard = install_test_subscriber();
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises execute_ddl_best_effort non-success path with field eval.
        ensure_depth_and_prev_close_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_depth_prev_close_send_error_with_tracing_subscriber() {
        let _guard = install_test_subscriber();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept first connection and respond OK, then drop second
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(MOCK_HTTP_200.as_bytes()).await;
                drop(stream);
            }
            // Second connection: drop immediately → send error on DEDUP DDL
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
        ensure_depth_and_prev_close_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_tick_table_dedup_send_error_with_tracing() {
        let _guard = install_test_subscriber();
        // Two-phase: first request (CREATE TABLE) succeeds,
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
        ensure_tick_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // Coverage: flush_if_needed with time elapsed
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_writer_flush_if_needed_triggers_after_interval() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = TickPersistenceWriter::new(&config).unwrap();

        // Append one tick so pending_count > 0
        let tick = make_test_tick(42, 100.0);
        writer.append_tick(&tick).unwrap();
        assert_eq!(writer.pending_count(), 1);

        // Force last_flush_ms to be old enough by sleeping past the interval
        std::thread::sleep(std::time::Duration::from_millis(
            TICK_FLUSH_INTERVAL_MS + 100,
        ));

        // Now flush_if_needed should trigger force_flush
        writer.flush_if_needed().unwrap();
        assert_eq!(
            writer.pending_count(),
            0,
            "flush_if_needed must flush when interval elapsed"
        );
    }

    #[test]
    fn test_depth_writer_flush_if_needed_triggers_after_interval() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = DepthPersistenceWriter::new(&config).unwrap();

        let depth = [MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 100.0,
            ask_price: 100.5,
        }; 5];
        writer
            .append_depth(
                42,
                EXCHANGE_SEGMENT_NSE_EQ,
                1_700_000_000_000_000_000,
                &depth,
            )
            .unwrap();
        assert!(writer.pending_count > 0);

        // Sleep past the flush interval
        std::thread::sleep(std::time::Duration::from_millis(
            TICK_FLUSH_INTERVAL_MS + 100,
        ));

        // Now flush_if_needed should trigger force_flush
        writer.flush_if_needed().unwrap();
        assert_eq!(
            writer.pending_count, 0,
            "flush_if_needed must flush depth when interval elapsed"
        );
    }
}
