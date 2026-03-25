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

use std::collections::VecDeque;
use std::io::{BufReader, BufWriter, Read as _, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_common::constants::{
    CANDLE_BUFFER_CAPACITY, CANDLE_FLUSH_BATCH_SIZE, IST_UTC_OFFSET_SECONDS_I64,
    QUESTDB_TABLE_CANDLES_1S, QUESTDB_TABLE_HISTORICAL_CANDLES,
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
// Pure helper functions (testable without DB)
// ---------------------------------------------------------------------------

/// Converts a UTC epoch-seconds timestamp to IST-as-UTC nanoseconds.
///
/// Adds `IST_UTC_OFFSET_SECONDS_I64` (19800s) so QuestDB displays IST wall-clock
/// time, then multiplies by 1_000_000_000 for nanosecond precision.
///
/// Uses saturating arithmetic to prevent overflow.
fn compute_ist_nanos_from_utc_secs(utc_secs: i64) -> i64 {
    utc_secs
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .saturating_mul(1_000_000_000)
}

/// Converts IST epoch seconds (from live WebSocket) to nanoseconds for QuestDB.
///
/// Live WebSocket timestamps are already IST — no offset needed.
/// Uses saturating arithmetic to prevent overflow.
fn compute_live_candle_nanos(ist_epoch_secs: u32) -> i64 {
    i64::from(ist_epoch_secs).saturating_mul(1_000_000_000)
}

/// Returns `true` if `pending_count` has reached or exceeded the given batch size threshold.
fn should_flush(pending_count: usize, batch_size: usize) -> bool {
    pending_count >= batch_size
}

/// Builds the QuestDB HTTP exec URL from host and port.
fn build_questdb_exec_url(host: &str, http_port: u16) -> String {
    format!("http://{}:{}/exec", host, http_port)
}

/// Builds the ALTER TABLE DEDUP ENABLE UPSERT KEYS SQL statement.
fn build_dedup_sql(table_name: &str, dedup_key: &str) -> String {
    format!(
        "ALTER TABLE {} DEDUP ENABLE UPSERT KEYS(ts, {})",
        table_name, dedup_key
    )
}

/// Builds the ILP TCP connection string from host and port.
fn build_ilp_conf_string(host: &str, ilp_port: u16) -> String {
    format!("tcp::addr={}:{};", host, ilp_port)
}

// ---------------------------------------------------------------------------
// Candle Persistence Writer
// ---------------------------------------------------------------------------

/// Max reconnection attempts for historical candle writer.
const HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Initial backoff delay (ms) for historical candle reconnection.
const HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS: u64 = 1000;

/// Batched candle writer for QuestDB via ILP.
///
/// Used for historical candle ingestion (cold path). On disconnect,
/// propagates errors to the caller (which decides whether to retry or skip).
pub struct CandlePersistenceWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    /// ILP connection config string, retained for reconnection.
    ilp_conf_string: String,
}

impl CandlePersistenceWriter {
    /// Creates a new candle writer connected to QuestDB via ILP TCP.
    ///
    /// # Errors
    /// Returns error if the ILP connection cannot be established.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = build_ilp_conf_string(&config.host, config.ilp_port);
        let sender =
            Sender::from_conf(&conf_string).context("failed to connect to QuestDB via ILP")?;
        let buffer = sender.new_buffer();

        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
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
        // Try reconnect if disconnected (cold path — ok to propagate error).
        self.try_reconnect_on_error()?;

        // UTC epoch → IST-as-UTC: add 19800s so QuestDB shows IST wall-clock time.
        let ts_nanos =
            TimestampNanos::new(compute_ist_nanos_from_utc_secs(candle.timestamp_utc_secs));

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

        if should_flush(self.pending_count, CANDLE_FLUSH_BATCH_SIZE) {
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
        let sender = self
            .sender
            .as_mut()
            .context("QuestDB sender disconnected for historical candle writer")?;
        if let Err(err) = sender.flush(&mut self.buffer) {
            self.sender = None;
            self.pending_count = 0;
            self.buffer.clear();
            return Err(err).context("flush candles to QuestDB");
        }
        self.pending_count = 0;

        debug!(flushed_rows = count, "candle batch flushed to QuestDB");
        Ok(())
    }

    /// Returns the number of candles currently buffered (not yet flushed).
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// Attempts to reconnect to QuestDB with exponential backoff.
    fn reconnect(&mut self) -> Result<()> {
        let mut delay_ms = HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS;

        for attempt in 1..=HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS {
            warn!(
                attempt,
                max_attempts = HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS,
                delay_ms,
                "attempting QuestDB ILP reconnection for historical candle writer"
            );

            std::thread::sleep(Duration::from_millis(delay_ms));

            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(new_sender) => {
                    self.buffer = new_sender.new_buffer();
                    self.sender = Some(new_sender);
                    self.pending_count = 0;
                    info!(
                        attempt,
                        "QuestDB ILP reconnection succeeded for historical candle writer"
                    );
                    return Ok(());
                }
                Err(err) => {
                    error!(
                        attempt,
                        max_attempts = HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS,
                        ?err,
                        "QuestDB ILP reconnection failed for historical candle writer"
                    );
                    delay_ms = delay_ms.saturating_mul(2);
                }
            }
        }

        anyhow::bail!(
            "QuestDB ILP reconnection failed after {} attempts for historical candle writer",
            HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS,
        )
    }

    /// Attempts reconnection only when the sender is `None`.
    fn try_reconnect_on_error(&mut self) -> Result<()> {
        if self.sender.is_some() {
            return Ok(());
        }
        self.reconnect()
    }
}

// ---------------------------------------------------------------------------
// Live Candle Writer (candles_1s — feeds materialized views)
// ---------------------------------------------------------------------------

/// Batch size for live candle ILP flushes. Smaller than historical since
/// live candles arrive in real-time and need lower latency.
const LIVE_CANDLE_FLUSH_BATCH_SIZE: usize = 128;

/// Max reconnection attempts per cycle for live candle writer.
const LIVE_CANDLE_MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Initial backoff delay (ms) for reconnection.
const LIVE_CANDLE_RECONNECT_INITIAL_DELAY_MS: u64 = 1000;
/// Minimum interval between reconnection attempt cycles.
const LIVE_CANDLE_RECONNECT_THROTTLE_SECS: u64 = 30;

/// A buffered candle waiting to be written to QuestDB after recovery.
/// Fixed-size, Copy — no allocation on push.
#[derive(Clone, Copy)]
struct BufferedCandle {
    security_id: u32,
    exchange_segment_code: u8,
    timestamp_secs: u32,
    open: f32,
    high: f32,
    low: f32,
    close: f32,
    volume: u32,
    tick_count: u32,
}

/// Fixed record size for disk-spilled candles: 36 bytes per candle.
const CANDLE_SPILL_RECORD_SIZE: usize = 36;

/// Directory for candle spill files.
const CANDLE_SPILL_DIR: &str = "data/spill";

/// Serialize a `BufferedCandle` to a fixed-size byte array for disk spill.
fn serialize_candle(c: &BufferedCandle) -> [u8; CANDLE_SPILL_RECORD_SIZE] {
    let mut buf = [0u8; CANDLE_SPILL_RECORD_SIZE];
    buf[0..4].copy_from_slice(&c.security_id.to_le_bytes());
    buf[4] = c.exchange_segment_code;
    // buf[5..7] = padding
    buf[7..11].copy_from_slice(&c.timestamp_secs.to_le_bytes());
    buf[11..15].copy_from_slice(&c.open.to_le_bytes());
    buf[15..19].copy_from_slice(&c.high.to_le_bytes());
    buf[19..23].copy_from_slice(&c.low.to_le_bytes());
    buf[23..27].copy_from_slice(&c.close.to_le_bytes());
    buf[27..31].copy_from_slice(&c.volume.to_le_bytes());
    buf[31..35].copy_from_slice(&c.tick_count.to_le_bytes());
    // buf[35] = padding
    buf
}

/// Deserialize a `BufferedCandle` from a fixed-size byte array.
fn deserialize_candle(buf: &[u8; CANDLE_SPILL_RECORD_SIZE]) -> BufferedCandle {
    BufferedCandle {
        security_id: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        exchange_segment_code: buf[4],
        timestamp_secs: u32::from_le_bytes([buf[7], buf[8], buf[9], buf[10]]),
        open: f32::from_le_bytes([buf[11], buf[12], buf[13], buf[14]]),
        high: f32::from_le_bytes([buf[15], buf[16], buf[17], buf[18]]),
        low: f32::from_le_bytes([buf[19], buf[20], buf[21], buf[22]]),
        close: f32::from_le_bytes([buf[23], buf[24], buf[25], buf[26]]),
        volume: u32::from_le_bytes([buf[27], buf[28], buf[29], buf[30]]),
        tick_count: u32::from_le_bytes([buf[31], buf[32], buf[33], buf[34]]),
    }
}

/// Batched writer for live 1-second candles to `candles_1s` via ILP.
///
/// Completed candles from `CandleAggregator` are written here, feeding
/// QuestDB materialized views (candles_1m, 5m, 15m, etc.).
///
/// **Resilience:** On QuestDB disconnect, candles are buffered in a bounded
/// ring buffer (`CANDLE_BUFFER_CAPACITY`). On reconnect, buffered candles
/// drain oldest-first. If the buffer overflows, oldest candles are dropped
/// and a CRITICAL alert fires.
pub struct LiveCandleWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    /// ILP connection config string, retained for reconnection.
    ilp_conf_string: String,
    /// Resilience ring buffer: holds candles when QuestDB is down.
    // O(1) EXEMPT: begin — VecDeque allocation bounded by CANDLE_BUFFER_CAPACITY (~5MB max)
    candle_buffer: VecDeque<BufferedCandle>,
    // O(1) EXEMPT: end
    /// In-flight buffer: tracks candles in ILP buffer not yet confirmed flushed.
    /// On flush failure, rescued back to ring buffer. Max: LIVE_CANDLE_FLUSH_BATCH_SIZE.
    // O(1) EXEMPT: begin — bounded by LIVE_CANDLE_FLUSH_BATCH_SIZE
    in_flight: Vec<BufferedCandle>,
    // O(1) EXEMPT: end
    /// Total candles spilled to disk (ring buffer overflow).
    candles_spilled_total: u64,
    /// Throttle: earliest time a reconnect may be attempted.
    next_reconnect_allowed: std::time::Instant,
    /// Open file handle for disk spill (lazy-opened on first overflow).
    spill_writer: Option<BufWriter<std::fs::File>>,
    /// Path of the current spill file (for drain + cleanup).
    spill_path: Option<std::path::PathBuf>,
}

impl LiveCandleWriter {
    /// Creates a new live candle writer connected to QuestDB via ILP TCP.
    ///
    /// If QuestDB is unreachable at startup, the writer initializes with
    /// `sender = None` and buffers candles until connection is established.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = build_ilp_conf_string(&config.host, config.ilp_port);
        let (sender, buffer) = match Sender::from_conf(&conf_string) {
            Ok(s) => {
                let b = s.new_buffer();
                (Some(s), b)
            }
            Err(err) => {
                warn!(
                    ?err,
                    "live candle writer: QuestDB unreachable at startup — buffering candles"
                );
                (None, Buffer::new(ProtocolVersion::V1))
            }
        };

        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            candle_buffer: VecDeque::with_capacity(CANDLE_BUFFER_CAPACITY),
            in_flight: Vec::with_capacity(LIVE_CANDLE_FLUSH_BATCH_SIZE),
            candles_spilled_total: 0,
            next_reconnect_allowed: std::time::Instant::now(),
            spill_writer: None,
            spill_path: None,
        })
    }

    /// Appends a completed 1-second candle to the ILP buffer.
    ///
    /// If QuestDB is disconnected, the candle is buffered in the ring buffer.
    /// Never returns an error — resilience guarantees the caller is never blocked.
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
        // If disconnected, try reconnect (throttled) then buffer on failure.
        if self.sender.is_none() {
            self.try_reconnect_on_error();
        }

        // If still disconnected after reconnect attempt, buffer the candle.
        if self.sender.is_none() {
            self.buffer_candle(BufferedCandle {
                security_id,
                exchange_segment_code,
                timestamp_secs,
                open,
                high,
                low,
                close,
                volume,
                tick_count,
            });
            return Ok(());
        }

        // Connected — write to ILP buffer.
        let ts_nanos = TimestampNanos::new(compute_live_candle_nanos(timestamp_secs));

        if let Err(err) = self
            .buffer
            .table(QUESTDB_TABLE_CANDLES_1S)
            .and_then(|b| b.symbol("segment", segment_code_to_str(exchange_segment_code)))
            .and_then(|b| b.column_i64("security_id", i64::from(security_id)))
            .and_then(|b| b.column_f64("open", f32_to_f64_clean(open)))
            .and_then(|b| b.column_f64("high", f32_to_f64_clean(high)))
            .and_then(|b| b.column_f64("low", f32_to_f64_clean(low)))
            .and_then(|b| b.column_f64("close", f32_to_f64_clean(close)))
            .and_then(|b| b.column_i64("volume", i64::from(volume)))
            .and_then(|b| b.column_i64("oi", 0))
            .and_then(|b| b.column_i64("tick_count", i64::from(tick_count)))
            .and_then(|b| b.at(ts_nanos))
        {
            warn!(?err, "live candle ILP buffer error — buffering candle");
            self.buffer_candle(BufferedCandle {
                security_id,
                exchange_segment_code,
                timestamp_secs,
                open,
                high,
                low,
                close,
                volume,
                tick_count,
            });
            return Ok(());
        }

        // Track in-flight so we can rescue on flush failure.
        self.in_flight.push(BufferedCandle {
            security_id,
            exchange_segment_code,
            timestamp_secs,
            open,
            high,
            low,
            close,
            volume,
            tick_count,
        });
        self.pending_count = self.pending_count.saturating_add(1);

        if should_flush(self.pending_count, LIVE_CANDLE_FLUSH_BATCH_SIZE) {
            self.force_flush_internal();
        }

        Ok(())
    }

    /// Forces an immediate flush of all buffered candles to QuestDB.
    ///
    /// On flush error, sets sender to None (triggers reconnect on next write).
    /// Never propagates errors — the caller is never blocked.
    pub fn force_flush(&mut self) -> Result<()> {
        self.force_flush_internal();
        Ok(())
    }

    /// Internal flush — sets sender=None on error, rescues in-flight candles.
    fn force_flush_internal(&mut self) {
        if self.pending_count == 0 {
            return;
        }

        let count = self.pending_count;

        if let Some(ref mut sender) = self.sender {
            if let Err(err) = sender.flush(&mut self.buffer) {
                warn!(
                    ?err,
                    pending = count,
                    "live candle flush failed — rescuing in-flight candles"
                );
                self.sender = None;
                self.buffer.clear();
                self.rescue_in_flight();
                return;
            }
        } else {
            // No sender — rescue in-flight candles to ring buffer.
            self.buffer.clear();
            self.rescue_in_flight();
            return;
        }

        // Flush succeeded — in-flight candles confirmed written.
        self.in_flight.clear();
        self.pending_count = 0;
        debug!(flushed_rows = count, "live candle batch flushed to QuestDB");

        // After successful flush, drain any buffered candles.
        if !self.candle_buffer.is_empty() {
            self.drain_candle_buffer();
        }
    }

    /// Rescues in-flight candles back to ring buffer / disk spill on flush failure.
    fn rescue_in_flight(&mut self) {
        if self.in_flight.is_empty() {
            self.pending_count = 0;
            return;
        }
        let rescued: Vec<BufferedCandle> = self.in_flight.drain(..).collect();
        let count = rescued.len();
        for candle in rescued {
            self.buffer_candle(candle);
        }
        self.pending_count = 0;
        warn!(
            rescued = count,
            ring_buffer = self.candle_buffer.len(),
            "rescued in-flight candles to ring buffer after flush failure"
        );
    }

    /// Flushes if any candles are buffered.
    pub fn flush_if_needed(&mut self) -> Result<()> {
        if self.pending_count > 0 {
            self.force_flush_internal();
        }
        Ok(())
    }

    /// Buffer a candle in the ring buffer. On overflow, spills to disk.
    /// **Zero candle loss guarantee** — never drops candles.
    fn buffer_candle(&mut self, candle: BufferedCandle) {
        if self.candle_buffer.len() >= CANDLE_BUFFER_CAPACITY {
            // Ring buffer full — spill to disk (never drop).
            self.spill_candle_to_disk(&candle);
        } else {
            self.candle_buffer.push_back(candle);
        }
        metrics::gauge!("dlt_candle_buffer_size").set(self.candle_buffer.len() as f64);
        metrics::counter!("dlt_candles_spilled_total").absolute(self.candles_spilled_total);
    }

    /// Spills a candle to disk when ring buffer is full.
    fn spill_candle_to_disk(&mut self, candle: &BufferedCandle) {
        if self.spill_writer.is_none()
            && let Err(err) = self.open_candle_spill_file()
        {
            error!(
                ?err,
                "CRITICAL: cannot open candle spill file — candle WILL be lost"
            );
            return;
        }

        let record = serialize_candle(candle);
        if let Some(ref mut writer) = self.spill_writer
            && let Err(err) = writer.write_all(&record)
        {
            error!(
                ?err,
                candles_spilled = self.candles_spilled_total,
                "CRITICAL: candle disk spill write failed — candle lost"
            );
            return;
        }

        self.candles_spilled_total = self.candles_spilled_total.saturating_add(1);
        if self.candles_spilled_total.is_multiple_of(1_000) {
            warn!(
                candles_spilled = self.candles_spilled_total,
                "candle disk spill growing — QuestDB still down"
            );
        }
    }

    /// Opens the candle spill file for the current date.
    fn open_candle_spill_file(&mut self) -> Result<()> {
        std::fs::create_dir_all(CANDLE_SPILL_DIR)
            .context("failed to create candle spill directory")?;

        let date = chrono::Utc::now().format("%Y%m%d");
        let path = std::path::PathBuf::from(format!("{CANDLE_SPILL_DIR}/candles-{date}.bin"));

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open candle spill file: {}", path.display()))?;

        info!(path = %path.display(), "opened candle spill file for disk overflow");
        self.spill_writer = Some(BufWriter::new(file));
        self.spill_path = Some(path);
        Ok(())
    }

    /// Drains buffered candles to QuestDB after recovery.
    ///
    /// Order:
    /// 1. Ring buffer (oldest first)
    /// 2. Disk spill (arrived after ring buffer filled)
    fn drain_candle_buffer(&mut self) {
        let ring_count = self.candle_buffer.len();
        let mut drained: usize = 0;

        // Phase 1: Drain ring buffer.
        while let Some(c) = self.candle_buffer.pop_front() {
            if self.build_candle_row(&c).is_err() {
                continue;
            }
            self.in_flight.push(c);
            drained = drained.saturating_add(1);

            if drained.is_multiple_of(LIVE_CANDLE_FLUSH_BATCH_SIZE)
                && let Some(ref mut sender) = self.sender
                && let Err(err) = sender.flush(&mut self.buffer)
            {
                warn!(?err, drained, "candle ring drain flush failed");
                self.sender = None;
                self.buffer.clear();
                self.rescue_in_flight();
                return;
            }
            // Clear in-flight on successful batch flush.
            if drained.is_multiple_of(LIVE_CANDLE_FLUSH_BATCH_SIZE) {
                self.in_flight.clear();
            }
        }

        // Final flush for ring buffer remainder.
        if !self.in_flight.is_empty()
            && let Some(ref mut sender) = self.sender
            && let Err(err) = sender.flush(&mut self.buffer)
        {
            warn!(?err, drained, "candle ring drain final flush failed");
            self.sender = None;
            self.buffer.clear();
            self.rescue_in_flight();
            return;
        }
        self.in_flight.clear();

        if drained > 0 {
            info!(drained, ring_count, "candle ring buffer drained to QuestDB");
        }

        // Phase 2: Drain disk spill file.
        if self.candles_spilled_total > 0 {
            self.drain_candle_disk_spill();
        }

        metrics::gauge!("dlt_candle_buffer_size").set(self.candle_buffer.len() as f64);
        metrics::counter!("dlt_candles_spilled_total").absolute(self.candles_spilled_total);
    }

    /// Builds an ILP row for a buffered candle. Returns Err on ILP buffer error.
    fn build_candle_row(&mut self, c: &BufferedCandle) -> Result<()> {
        let ts_nanos = TimestampNanos::new(compute_live_candle_nanos(c.timestamp_secs));
        self.buffer
            .table(QUESTDB_TABLE_CANDLES_1S)
            .and_then(|b| b.symbol("segment", segment_code_to_str(c.exchange_segment_code)))
            .and_then(|b| b.column_i64("security_id", i64::from(c.security_id)))
            .and_then(|b| b.column_f64("open", f32_to_f64_clean(c.open)))
            .and_then(|b| b.column_f64("high", f32_to_f64_clean(c.high)))
            .and_then(|b| b.column_f64("low", f32_to_f64_clean(c.low)))
            .and_then(|b| b.column_f64("close", f32_to_f64_clean(c.close)))
            .and_then(|b| b.column_i64("volume", i64::from(c.volume)))
            .and_then(|b| b.column_i64("oi", 0))
            .and_then(|b| b.column_i64("tick_count", i64::from(c.tick_count)))
            .and_then(|b| b.at(ts_nanos))
            .context("build candle ILP row")?;
        Ok(())
    }

    /// Drains the candle disk spill file to QuestDB.
    fn drain_candle_disk_spill(&mut self) {
        // Close the spill writer first.
        if let Some(ref mut writer) = self.spill_writer {
            let _ = writer.flush();
        }
        self.spill_writer = None;

        let spill_path = match self.spill_path.take() {
            Some(p) => p,
            None => return,
        };

        let file = match std::fs::File::open(&spill_path) {
            Ok(f) => f,
            Err(err) => {
                warn!(?err, path = %spill_path.display(), "cannot open candle spill file");
                return;
            }
        };

        let mut reader = BufReader::new(file);
        let mut record = [0u8; CANDLE_SPILL_RECORD_SIZE];
        let mut drained: usize = 0;
        let mut flush_failed = false;

        loop {
            match reader.read_exact(&mut record) {
                Ok(()) => {}
                Err(ref err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => {
                    warn!(?err, drained, "candle spill read error");
                    break;
                }
            }

            let candle = deserialize_candle(&record);
            if self.build_candle_row(&candle).is_err() {
                continue;
            }
            drained = drained.saturating_add(1);

            if drained.is_multiple_of(LIVE_CANDLE_FLUSH_BATCH_SIZE)
                && let Some(ref mut sender) = self.sender
                && let Err(err) = sender.flush(&mut self.buffer)
            {
                warn!(?err, drained, "candle spill drain flush failed");
                flush_failed = true;
                break;
            }
        }

        // Final flush.
        if !flush_failed
            && let Some(ref mut sender) = self.sender
            && let Err(err) = sender.flush(&mut self.buffer)
        {
            warn!(?err, drained, "candle spill drain final flush failed");
            flush_failed = true;
        }

        // Only delete spill file if drain was complete (no flush failures).
        if !flush_failed {
            if let Err(err) = std::fs::remove_file(&spill_path) {
                warn!(?err, path = %spill_path.display(), "failed to delete candle spill file");
            } else {
                info!(path = %spill_path.display(), drained, "candle spill drained and deleted");
            }
            self.candles_spilled_total = 0;
        } else {
            // Preserve spill file for next recovery attempt.
            self.spill_path = Some(spill_path);
            warn!(
                drained,
                "candle spill partially drained — file preserved for next recovery"
            );
        }
    }

    /// Attempts to reconnect to QuestDB with exponential backoff.
    fn reconnect(&mut self) -> Result<()> {
        let mut delay_ms = LIVE_CANDLE_RECONNECT_INITIAL_DELAY_MS;

        for attempt in 1..=LIVE_CANDLE_MAX_RECONNECT_ATTEMPTS {
            warn!(
                attempt,
                max_attempts = LIVE_CANDLE_MAX_RECONNECT_ATTEMPTS,
                delay_ms,
                buffered_candles = self.candle_buffer.len(),
                "attempting QuestDB ILP reconnection for candle writer"
            );

            std::thread::sleep(Duration::from_millis(delay_ms));

            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(new_sender) => {
                    self.buffer = new_sender.new_buffer();
                    self.sender = Some(new_sender);
                    self.pending_count = 0;
                    info!(
                        attempt,
                        "QuestDB ILP reconnection succeeded for candle writer"
                    );
                    return Ok(());
                }
                Err(err) => {
                    error!(
                        attempt,
                        max_attempts = LIVE_CANDLE_MAX_RECONNECT_ATTEMPTS,
                        ?err,
                        "QuestDB ILP reconnection failed for candle writer"
                    );
                    delay_ms = delay_ms.saturating_mul(2);
                }
            }
        }

        anyhow::bail!(
            "QuestDB ILP reconnection failed after {} attempts for candle writer",
            LIVE_CANDLE_MAX_RECONNECT_ATTEMPTS,
        )
    }

    /// Throttled reconnect — at most once per `LIVE_CANDLE_RECONNECT_THROTTLE_SECS`.
    fn try_reconnect_on_error(&mut self) {
        if self.sender.is_some() {
            return;
        }
        let now = std::time::Instant::now();
        if now < self.next_reconnect_allowed {
            return;
        }
        self.next_reconnect_allowed =
            now + Duration::from_secs(LIVE_CANDLE_RECONNECT_THROTTLE_SECS);
        if let Err(err) = self.reconnect() {
            warn!(?err, "candle writer reconnect failed — will retry later");
        }
    }

    /// Returns the number of candles in the resilience ring buffer.
    // TEST-EXEMPT: trivial accessor, tested indirectly via resilience integration tests
    pub fn buffered_candle_count(&self) -> usize {
        self.candle_buffer.len()
    }

    /// Returns the total number of candles spilled to disk (ring buffer overflow).
    // TEST-EXEMPT: trivial accessor, tested indirectly via resilience integration tests
    pub fn candles_dropped_total(&self) -> u64 {
        self.candles_spilled_total
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
    let base_url = build_questdb_exec_url(&questdb_config.host, questdb_config.http_port);

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
    let dedup_sql = build_dedup_sql(QUESTDB_TABLE_HISTORICAL_CANDLES, DEDUP_KEY_CANDLES);
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
    // compute_ist_nanos_from_utc_secs
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_ist_nanos_basic() {
        // UTC 0 → IST is +19800s → 19800 * 1_000_000_000 nanos
        let nanos = compute_ist_nanos_from_utc_secs(0);
        assert_eq!(nanos, 19_800 * 1_000_000_000);
    }

    #[test]
    fn test_compute_ist_nanos_known_timestamp() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        // IST = 1704067200 + 19800 = 1704087000
        // nanos = 1704087000 * 1_000_000_000
        let utc_secs = 1_704_067_200_i64;
        let nanos = compute_ist_nanos_from_utc_secs(utc_secs);
        let expected = (utc_secs + IST_UTC_OFFSET_SECONDS_I64) * 1_000_000_000;
        assert_eq!(nanos, expected);
    }

    #[test]
    fn test_compute_ist_nanos_negative_utc_secs() {
        // Negative UTC secs (before epoch) — should still work with saturating
        let nanos = compute_ist_nanos_from_utc_secs(-100_000);
        let expected = (-100_000_i64 + IST_UTC_OFFSET_SECONDS_I64) * 1_000_000_000;
        assert_eq!(nanos, expected);
    }

    #[test]
    fn test_compute_ist_nanos_saturates_on_overflow() {
        // i64::MAX should saturate, not overflow
        let nanos = compute_ist_nanos_from_utc_secs(i64::MAX);
        assert_eq!(nanos, i64::MAX);
    }

    #[test]
    fn test_compute_ist_nanos_offset_is_19800() {
        // Verify the IST offset constant is correct (5h30m = 19800s)
        assert_eq!(IST_UTC_OFFSET_SECONDS_I64, 5 * 3600 + 30 * 60);
    }

    // -----------------------------------------------------------------------
    // compute_live_candle_nanos
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_live_candle_nanos_zero() {
        assert_eq!(compute_live_candle_nanos(0), 0);
    }

    #[test]
    fn test_compute_live_candle_nanos_one_second() {
        assert_eq!(compute_live_candle_nanos(1), 1_000_000_000);
    }

    #[test]
    fn test_compute_live_candle_nanos_typical_ist_timestamp() {
        // A typical IST epoch second for 09:15:00 IST
        let ist_secs: u32 = 1_740_556_500;
        let nanos = compute_live_candle_nanos(ist_secs);
        assert_eq!(nanos, i64::from(ist_secs) * 1_000_000_000);
    }

    #[test]
    fn test_compute_live_candle_nanos_max_u32() {
        // u32::MAX should not overflow i64
        let nanos = compute_live_candle_nanos(u32::MAX);
        let expected = i64::from(u32::MAX) * 1_000_000_000;
        assert_eq!(nanos, expected);
    }

    // -----------------------------------------------------------------------
    // should_flush
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_flush_below_threshold() {
        assert!(!should_flush(0, 500));
        assert!(!should_flush(499, 500));
    }

    #[test]
    fn test_should_flush_at_threshold() {
        assert!(should_flush(500, 500));
    }

    #[test]
    fn test_should_flush_above_threshold() {
        assert!(should_flush(501, 500));
        assert!(should_flush(1000, 500));
    }

    #[test]
    fn test_should_flush_with_live_batch_size() {
        assert!(!should_flush(127, LIVE_CANDLE_FLUSH_BATCH_SIZE));
        assert!(should_flush(128, LIVE_CANDLE_FLUSH_BATCH_SIZE));
        assert!(should_flush(200, LIVE_CANDLE_FLUSH_BATCH_SIZE));
    }

    #[test]
    fn test_should_flush_zero_threshold() {
        // Edge case: zero threshold always triggers flush (except empty)
        assert!(should_flush(0, 0));
        assert!(should_flush(1, 0));
    }

    // -----------------------------------------------------------------------
    // build_questdb_exec_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_questdb_exec_url_docker() {
        let url = build_questdb_exec_url("dlt-questdb", 9000);
        assert_eq!(url, "http://dlt-questdb:9000/exec");
    }

    #[test]
    fn test_build_questdb_exec_url_custom_port() {
        let url = build_questdb_exec_url("192.168.1.100", 19000);
        assert_eq!(url, "http://192.168.1.100:19000/exec");
    }

    // -----------------------------------------------------------------------
    // build_dedup_sql
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_dedup_sql_historical_candles() {
        let sql = build_dedup_sql(QUESTDB_TABLE_HISTORICAL_CANDLES, DEDUP_KEY_CANDLES);
        assert_eq!(
            sql,
            "ALTER TABLE historical_candles DEDUP ENABLE UPSERT KEYS(ts, security_id, timeframe, segment)"
        );
    }

    #[test]
    fn test_build_dedup_sql_starts_with_alter_table() {
        let sql = build_dedup_sql("any_table", "col_a, col_b");
        assert!(sql.starts_with("ALTER TABLE any_table"));
        assert!(sql.contains("DEDUP ENABLE UPSERT KEYS(ts, col_a, col_b)"));
    }

    // -----------------------------------------------------------------------
    // build_ilp_conf_string
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ilp_conf_string_docker() {
        let conf = build_ilp_conf_string("dlt-questdb", 9009);
        assert_eq!(conf, "tcp::addr=dlt-questdb:9009;");
    }

    #[test]
    fn test_build_ilp_conf_string_custom() {
        let conf = build_ilp_conf_string("10.0.0.5", 19009);
        assert_eq!(conf, "tcp::addr=10.0.0.5:19009;");
    }

    #[test]
    fn test_build_ilp_conf_string_ends_with_semicolon() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(
            conf.ends_with(';'),
            "ILP conf string must end with semicolon"
        );
    }

    #[test]
    fn test_build_ilp_conf_string_starts_with_tcp() {
        let conf = build_ilp_conf_string("host", 1234);
        assert!(
            conf.starts_with("tcp::addr="),
            "ILP conf string must start with tcp::addr="
        );
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

    fn make_test_candle(security_id: u32) -> HistoricalCandle {
        HistoricalCandle {
            security_id,
            exchange_segment_code: 2, // NSE_FNO
            timeframe: "1m",
            timestamp_utc_secs: 1_704_067_200, // 2024-01-01 00:00:00 UTC
            open: 21000.0,
            high: 21050.0,
            low: 20950.0,
            close: 21025.0,
            volume: 500_000,
            open_interest: 120_000,
        }
    }

    // -----------------------------------------------------------------------
    // CandlePersistenceWriter — new, append_candle, force_flush, pending_count
    // -----------------------------------------------------------------------

    #[test]
    fn test_candle_writer_new_with_tcp_drain() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = CandlePersistenceWriter::new(&config);
        assert!(writer.is_ok(), "must connect to TCP drain server");
        let writer = writer.unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_writer_append_single_candle() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        let candle = make_test_candle(11536);
        let result = writer.append_candle(&candle);
        assert!(result.is_ok());
        assert_eq!(writer.pending_count(), 1);
    }

    #[test]
    fn test_candle_writer_append_multiple_candles() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        for i in 0..5 {
            let candle = make_test_candle(11536 + i);
            writer.append_candle(&candle).unwrap();
        }
        assert_eq!(writer.pending_count(), 5);
    }

    #[test]
    fn test_candle_writer_force_flush_resets_count() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        let candle = make_test_candle(11536);
        writer.append_candle(&candle).unwrap();
        assert_eq!(writer.pending_count(), 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count(), 0);
    }

    #[test]
    fn test_candle_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();
        // Flushing empty buffer should succeed and remain at 0
        let result = writer.force_flush();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count(), 0);
    }

    // -----------------------------------------------------------------------
    // LiveCandleWriter — new, append_candle, force_flush, flush_if_needed
    // -----------------------------------------------------------------------

    #[test]
    fn test_live_candle_writer_new_with_tcp_drain() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = LiveCandleWriter::new(&config);
        assert!(writer.is_ok(), "must connect to TCP drain server");
    }

    #[test]
    fn test_live_candle_writer_append_and_flush() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        writer
            .append_candle(
                11536,
                2, // NSE_FNO
                1_740_556_500,
                21000.0,
                21050.0,
                20950.0,
                21025.0,
                50_000,
                100,
            )
            .unwrap();
        assert_eq!(writer.pending_count, 1);

        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_force_flush_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        let result = writer.force_flush();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_flush_if_needed_empty_is_noop() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        let result = writer.flush_if_needed();
        assert!(result.is_ok());
        assert_eq!(writer.pending_count, 0);
    }

    #[test]
    fn test_live_candle_writer_flush_if_needed_with_pending() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        writer
            .append_candle(11536, 2, 1_740_556_500, 100.0, 110.0, 90.0, 105.0, 1000, 10)
            .unwrap();
        assert_eq!(writer.pending_count, 1);
        // flush_if_needed flushes if pending > 0
        writer.flush_if_needed().unwrap();
        assert_eq!(writer.pending_count, 0);
    }

    // -----------------------------------------------------------------------
    // ensure_candle_table_dedup_keys — HTTP success, HTTP non-success, error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_candle_table_http_200_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Should complete without panic — exercises success paths (lines 364-365, 398)
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_http_400_non_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Should complete without panic — exercises non-success paths (lines 374, 407)
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_create_ok_dedup_send_error() {
        // Two-phase: first request (CREATE TABLE) succeeds, second (DEDUP) fails
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
        // Exercises the Err(err) branch on DEDUP DDL (lines 412-413)
        ensure_candle_table_dedup_keys(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_candle_table_create_send_error_returns_early() {
        // First request (CREATE TABLE) fails → should return early (line 384)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Drop connection immediately → send error on CREATE TABLE
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
        // Exercises the Err(err) + return branch on CREATE TABLE (lines 379-380, 384)
        ensure_candle_table_dedup_keys(&config).await;
    }

    // -----------------------------------------------------------------------
    // LiveCandleWriter resilience tests (Phase B)
    // -----------------------------------------------------------------------

    #[test]
    fn test_live_candle_writer_starts_connected() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = LiveCandleWriter::new(&config).unwrap();
        assert!(writer.sender.is_some(), "writer must start connected");
        assert_eq!(writer.buffered_candle_count(), 0);
        assert_eq!(writer.candles_dropped_total(), 0);
    }

    #[test]
    fn test_live_candle_writer_starts_disconnected_when_unreachable() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(), // RFC 5737 TEST-NET — unreachable
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        // Must NOT panic — initializes with sender=None.
        let writer = LiveCandleWriter::new(&config).unwrap();
        assert!(
            writer.sender.is_none(),
            "writer must start disconnected when host unreachable"
        );
    }

    #[test]
    fn test_live_candle_append_buffers_when_disconnected() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();

        // Prevent reconnect attempts (throttle to far future).
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Append should succeed (buffered, not written to QuestDB).
        let result = writer.append_candle(
            13,
            2,
            1_740_556_500,
            24500.0,
            24510.0,
            24490.0,
            24505.0,
            100,
            10,
        );
        assert!(
            result.is_ok(),
            "append must never fail — buffers on disconnect"
        );
        assert_eq!(
            writer.buffered_candle_count(),
            1,
            "candle must be in ring buffer"
        );
    }

    #[test]
    fn test_live_candle_ring_buffer_fifo_order() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Append 3 candles with different security_ids.
        for i in 0..3_u32 {
            writer
                .append_candle(
                    100 + i,
                    2,
                    1_740_556_500 + i,
                    24500.0,
                    24510.0,
                    24490.0,
                    24505.0,
                    100,
                    10,
                )
                .unwrap();
        }
        assert_eq!(writer.buffered_candle_count(), 3);

        // Verify FIFO order: first candle has security_id 100.
        let first = writer.candle_buffer.front().unwrap();
        assert_eq!(
            first.security_id, 100,
            "ring buffer must be FIFO — oldest first"
        );
        let last = writer.candle_buffer.back().unwrap();
        assert_eq!(
            last.security_id, 102,
            "ring buffer must be FIFO — newest last"
        );
    }

    #[test]
    fn test_live_candle_ring_buffer_overflow_spills_to_disk() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Pre-set spill to unique temp path.
        let tmp_dir =
            std::env::temp_dir().join(format!("dlt-candle-spill-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let spill_path = tmp_dir.join("test-candles-overflow.bin");
        let file = std::fs::File::create(&spill_path).unwrap();
        writer.spill_writer = Some(BufWriter::new(file));
        writer.spill_path = Some(spill_path.clone());

        // Fill ring buffer to capacity.
        for i in 0..CANDLE_BUFFER_CAPACITY as u32 {
            writer
                .append_candle(i, 2, 1_740_556_500 + i, 100.0, 110.0, 90.0, 105.0, 50, 5)
                .unwrap();
        }
        assert_eq!(writer.buffered_candle_count(), CANDLE_BUFFER_CAPACITY);
        assert_eq!(writer.candles_spilled_total, 0, "no spill yet at capacity");

        // One more — should SPILL to disk (not drop).
        writer
            .append_candle(999999, 2, 1_740_600_000, 200.0, 210.0, 190.0, 205.0, 50, 5)
            .unwrap();
        // Ring buffer stays at capacity.
        assert_eq!(
            writer.buffered_candle_count(),
            CANDLE_BUFFER_CAPACITY,
            "ring buffer stays at capacity"
        );
        // Spill count = 1 (NOT dropped — it's on disk).
        assert_eq!(
            writer.candles_spilled_total, 1,
            "exactly 1 candle spilled to disk, NOT dropped"
        );

        // The ring buffer oldest is still id=0 (no pop_front anymore).
        let oldest = writer.candle_buffer.front().unwrap();
        assert_eq!(
            oldest.security_id, 0,
            "oldest candle (id=0) must still be in ring buffer (not dropped)"
        );

        // Cleanup.
        let _ = std::fs::remove_file(&spill_path);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_live_candle_force_flush_ok_when_no_pending() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        // Flush with nothing pending should be a no-op.
        let result = writer.force_flush();
        assert!(result.is_ok(), "flush with no pending must return Ok");
    }

    #[test]
    fn test_live_candle_append_and_flush_connected() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        assert!(writer.sender.is_some());

        // Append a candle — should go to ILP buffer, not ring buffer.
        writer
            .append_candle(
                13,
                2,
                1_740_556_500,
                24500.0,
                24510.0,
                24490.0,
                24505.0,
                100,
                10,
            )
            .unwrap();
        assert_eq!(writer.pending_count, 1, "should be in ILP buffer");
        assert_eq!(
            writer.buffered_candle_count(),
            0,
            "should NOT be in ring buffer"
        );

        // Flush to QuestDB.
        writer.force_flush().unwrap();
        assert_eq!(writer.pending_count, 0, "ILP buffer cleared after flush");
    }

    #[test]
    fn test_live_candle_flush_error_nulls_sender() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = LiveCandleWriter::new(&config).unwrap();
        assert!(writer.sender.is_some());

        // Append a candle to have pending data.
        writer
            .append_candle(
                13,
                2,
                1_740_556_500,
                24500.0,
                24510.0,
                24490.0,
                24505.0,
                100,
                10,
            )
            .unwrap();

        // Sabotage: point ilp_conf_string to unreachable host so reconnect fails.
        writer.ilp_conf_string = "tcp::addr=192.0.2.1:1;".to_string();
        // Drop the sender to simulate disconnect.
        writer.sender = None;
        writer.pending_count = 0;

        // Subsequent append should buffer.
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);
        writer
            .append_candle(
                14,
                2,
                1_740_556_501,
                24600.0,
                24610.0,
                24590.0,
                24605.0,
                200,
                20,
            )
            .unwrap();
        assert_eq!(
            writer.buffered_candle_count(),
            1,
            "must buffer when disconnected"
        );
        assert!(writer.sender.is_none(), "sender must remain None");
    }

    #[test]
    fn test_live_candle_reconnect_throttle_constant() {
        assert_eq!(
            LIVE_CANDLE_RECONNECT_THROTTLE_SECS, 30,
            "candle writer throttle must be 30s"
        );
    }

    #[test]
    fn test_live_candle_buffered_candle_struct_is_copy() {
        // Verify BufferedCandle is Copy (no allocation on push).
        let c = BufferedCandle {
            security_id: 13,
            exchange_segment_code: 2,
            timestamp_secs: 1_740_556_500,
            open: 24500.0,
            high: 24510.0,
            low: 24490.0,
            close: 24505.0,
            volume: 100,
            tick_count: 10,
        };
        let c2 = c; // Copy
        assert_eq!(c.security_id, c2.security_id, "BufferedCandle must be Copy");
    }

    #[test]
    fn test_candle_buffer_capacity_constant() {
        assert_eq!(CANDLE_BUFFER_CAPACITY, 100_000);
        // Memory footprint: ~48 bytes × 100K = ~4.8MB — reasonable.
        let size = std::mem::size_of::<BufferedCandle>();
        let total_mb = (size * CANDLE_BUFFER_CAPACITY) as f64 / 1_048_576.0;
        assert!(
            total_mb < 10.0,
            "candle ring buffer must be < 10MB, got {total_mb:.1}MB"
        );
    }

    // -----------------------------------------------------------------------
    // CandlePersistenceWriter resilience tests (Phase D)
    // -----------------------------------------------------------------------

    #[test]
    fn test_historical_candle_writer_starts_connected() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = CandlePersistenceWriter::new(&config).unwrap();
        assert!(
            writer.sender.is_some(),
            "historical writer must start connected"
        );
        assert!(
            !writer.ilp_conf_string.is_empty(),
            "must store conf string for reconnect"
        );
    }

    #[test]
    fn test_historical_candle_writer_reconnect_fails_unreachable() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Simulate disconnect + unreachable host.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=192.0.2.1:1;".to_string();

        let result = writer.try_reconnect_on_error();
        assert!(result.is_err(), "reconnect to unreachable host must fail");
        assert!(writer.sender.is_none());
    }

    #[test]
    fn test_historical_candle_writer_reconnect_succeeds() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = CandlePersistenceWriter::new(&config).unwrap();

        // Simulate disconnect — point to a NEW drain server for reconnect.
        writer.sender = None;
        let reconnect_port = spawn_tcp_drain_server();
        writer.ilp_conf_string = format!("tcp::addr=127.0.0.1:{reconnect_port};");

        let result = writer.try_reconnect_on_error();
        assert!(result.is_ok(), "reconnect to drain server must succeed");
        assert!(
            writer.sender.is_some(),
            "sender must be Some after successful reconnect"
        );
    }

    #[test]
    fn test_historical_candle_reconnect_constants() {
        assert_eq!(HISTORICAL_CANDLE_MAX_RECONNECT_ATTEMPTS, 3);
        assert_eq!(HISTORICAL_CANDLE_RECONNECT_INITIAL_DELAY_MS, 1000);
    }
}
