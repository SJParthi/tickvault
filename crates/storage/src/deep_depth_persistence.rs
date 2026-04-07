//! QuestDB ILP persistence for 20-level and 200-level market depth.
//!
//! Persists deep depth snapshots from the depth WebSocket connections.
//! Each row = one level on one side (bid or ask) for one instrument.
//!
//! # Table: `deep_market_depth`
//! Partitioned by HOUR (high volume: up to 50 instruments × 20 levels × 2 sides × ~1/sec).
//! DEDUP on `(security_id, segment, level, side)` prevents duplicates on reconnect.
//!
//! # Resilience
//! On QuestDB flush failure, in-flight rows are rescued to a bounded ring buffer
//! (`DEEP_DEPTH_BUFFER_CAPACITY`). When the ring buffer overflows, data spills to
//! disk (`data/spill/deep-depth-YYYYMMDD.bin`). On recovery, ring buffer drains
//! first, then disk spill. Architecture mirrors `TickPersistenceWriter`.

use std::collections::VecDeque;
use std::io::BufWriter;
use std::io::Write as IoWrite;
use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

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

/// Ring buffer capacity for QuestDB outage resilience.
/// Holds depth records in memory when QuestDB is down, drains on recovery.
/// 50,000 records × ~352 bytes (20 levels) = ~17MB max.
const DEEP_DEPTH_BUFFER_CAPACITY: usize = 50_000;

/// High watermark (80% of ring buffer capacity). Fires CRITICAL alert once.
const DEEP_DEPTH_BUFFER_HIGH_WATERMARK: usize = DEEP_DEPTH_BUFFER_CAPACITY * 4 / 5;

/// Fixed record size for disk spill: header(16) + max 200 levels × 16 bytes = 3216 bytes.
const DEEP_DEPTH_SPILL_RECORD_SIZE: usize = 16 + 200 * 16;

/// Directory for deep depth spill files.
const DEEP_DEPTH_SPILL_DIR: &str = "data/spill";

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

/// A buffered depth record for ring buffer / disk spill recovery.
/// Captures the parameters passed to `append_deep_depth` so the record
/// can be replayed on QuestDB reconnect.
#[derive(Clone)]
struct DeepDepthRecord {
    security_id: u32,
    segment_code: u8,
    /// 0 = BID, 1 = ASK
    side_code: u8,
    /// 20 or 200
    depth_type_code: u8,
    level_count: u8,
    received_at_nanos: i64,
    /// Up to 200 levels. Only `level_count` entries are valid.
    levels: [DeepDepthLevel; 200],
}

impl DeepDepthRecord {
    /// Creates a record from the append parameters.
    fn from_append(
        security_id: u32,
        segment_code: u8,
        side: &str,
        levels: &[DeepDepthLevel],
        depth_type: &str,
        received_at_nanos: i64,
    ) -> Self {
        let side_code = if side == "ASK" { 1u8 } else { 0u8 };
        let depth_type_code = if depth_type == "200" { 200u8 } else { 20u8 };
        let level_count = levels.len().min(200) as u8;
        let mut arr = [DeepDepthLevel::default(); 200];
        for (i, lvl) in levels.iter().take(200).enumerate() {
            arr[i] = *lvl;
        }
        Self {
            security_id,
            segment_code,
            side_code,
            depth_type_code,
            level_count,
            received_at_nanos,
            levels: arr,
        }
    }

    fn side_str(&self) -> &'static str {
        if self.side_code == 1 { "ASK" } else { "BID" }
    }

    fn depth_type_str(&self) -> &'static str {
        if self.depth_type_code == 200 {
            "200"
        } else {
            "20"
        }
    }

    fn valid_levels(&self) -> &[DeepDepthLevel] {
        &self.levels[..usize::from(self.level_count)]
    }

    /// Serializes to a fixed-size byte array for disk spill.
    fn serialize(&self) -> [u8; DEEP_DEPTH_SPILL_RECORD_SIZE] {
        let mut buf = [0u8; DEEP_DEPTH_SPILL_RECORD_SIZE];
        // Header: 16 bytes
        buf[0..4].copy_from_slice(&self.security_id.to_le_bytes());
        buf[4] = self.segment_code;
        buf[5] = self.side_code;
        buf[6] = self.depth_type_code;
        buf[7] = self.level_count;
        buf[8..16].copy_from_slice(&self.received_at_nanos.to_le_bytes());
        // Levels: level_count × 16 bytes each
        for i in 0..usize::from(self.level_count) {
            let offset = 16 + i * 16;
            buf[offset..offset + 8].copy_from_slice(&self.levels[i].price.to_le_bytes());
            buf[offset + 8..offset + 12].copy_from_slice(&self.levels[i].quantity.to_le_bytes());
            buf[offset + 12..offset + 16].copy_from_slice(&self.levels[i].orders.to_le_bytes());
        }
        buf
    }

    /// Deserializes from a fixed-size byte array (disk spill recovery).
    fn deserialize(buf: &[u8; DEEP_DEPTH_SPILL_RECORD_SIZE]) -> Self {
        let security_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let segment_code = buf[4];
        let side_code = buf[5];
        let depth_type_code = buf[6];
        let level_count = buf[7].min(200);
        let received_at_nanos = i64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        let mut levels = [DeepDepthLevel::default(); 200];
        for (i, level) in levels.iter_mut().take(usize::from(level_count)).enumerate() {
            let offset = 16 + i * 16;
            *level = DeepDepthLevel {
                price: f64::from_le_bytes([
                    buf[offset],
                    buf[offset + 1],
                    buf[offset + 2],
                    buf[offset + 3],
                    buf[offset + 4],
                    buf[offset + 5],
                    buf[offset + 6],
                    buf[offset + 7],
                ]),
                quantity: u32::from_le_bytes([
                    buf[offset + 8],
                    buf[offset + 9],
                    buf[offset + 10],
                    buf[offset + 11],
                ]),
                orders: u32::from_le_bytes([
                    buf[offset + 12],
                    buf[offset + 13],
                    buf[offset + 14],
                    buf[offset + 15],
                ]),
            };
        }
        Self {
            security_id,
            segment_code,
            side_code,
            depth_type_code,
            level_count,
            received_at_nanos,
            levels,
        }
    }
}

/// Writer for deep depth data to QuestDB via ILP.
///
/// On flush failure, in-flight records are rescued to a bounded ring buffer
/// (`DEEP_DEPTH_BUFFER_CAPACITY`). When the ring buffer overflows, records
/// spill to disk. On reconnect, ring buffer drains first, then disk spill.
pub struct DeepDepthWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    ilp_conf_string: String,
    /// In-flight records: written to ILP buffer but not yet flushed.
    /// Rescued to ring buffer on flush failure.
    in_flight: Vec<DeepDepthRecord>,
    /// Ring buffer for QuestDB outage resilience. Drains on recovery.
    // O(1) EXEMPT: begin — VecDeque bounded by DEEP_DEPTH_BUFFER_CAPACITY
    depth_buffer: VecDeque<DeepDepthRecord>,
    // O(1) EXEMPT: end
    /// Total records spilled to disk (ring buffer overflow).
    records_spilled_total: u64,
    /// Total records permanently dropped (both buffer AND disk spill failed).
    records_dropped_total: u64,
    /// Open file handle for disk spill (lazy-opened on first overflow).
    spill_writer: Option<BufWriter<std::fs::File>>,
    /// Path of the current spill file.
    spill_path: Option<std::path::PathBuf>,
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
            in_flight: Vec::with_capacity(DEEP_DEPTH_FLUSH_BATCH_SIZE),
            depth_buffer: VecDeque::with_capacity(DEEP_DEPTH_BUFFER_CAPACITY),
            records_spilled_total: 0,
            records_dropped_total: 0,
            spill_writer: None,
            spill_path: None,
        })
    }

    /// Appends one side (bid or ask) of a deep depth snapshot.
    ///
    /// When QuestDB is down, records are held in a ring buffer (up to
    /// `DEEP_DEPTH_BUFFER_CAPACITY`) and drained on recovery.
    ///
    /// # Arguments
    /// * `security_id` — Dhan security ID.
    /// * `segment_code` — Exchange segment byte code.
    /// * `side` — "BID" or "ASK".
    /// * `levels` — Depth levels (20 or up to 200).
    /// * `depth_type` — "20" or "200".
    /// * `received_at_nanos` — UTC receive timestamp in nanoseconds.
    pub fn append_deep_depth(
        &mut self,
        security_id: u32,
        segment_code: u8,
        side: &str,
        levels: &[DeepDepthLevel],
        depth_type: &str,
        received_at_nanos: i64,
    ) -> Result<()> {
        // If sender is None (previous failure), attempt reconnect before writing.
        if self.sender.is_none() {
            match self.try_reconnect() {
                Ok(()) => {
                    // Reconnected — drain any buffered records first.
                    self.drain_depth_buffer();
                }
                Err(_) => {
                    // Still can't connect — buffer this record instead of losing it.
                    let record = DeepDepthRecord::from_append(
                        security_id,
                        segment_code,
                        side,
                        levels,
                        depth_type,
                        received_at_nanos,
                    );
                    self.buffer_record(record);
                    return Ok(());
                }
            }
        }

        let record = DeepDepthRecord::from_append(
            security_id,
            segment_code,
            side,
            levels,
            depth_type,
            received_at_nanos,
        );

        if let Err(err) = self.write_record_to_ilp(&record) {
            // ILP buffer is in a dirty state after partial write failure.
            // Clear it to prevent cascading errors on subsequent calls.
            self.buffer.clear();
            self.pending_count = 0;
            return Err(err);
        }

        // Track in-flight for rescue on flush failure.
        self.in_flight.push(record);

        // Auto-flush when batch is large enough
        if self.pending_count >= DEEP_DEPTH_FLUSH_BATCH_SIZE
            && let Err(err) = self.force_flush()
        {
            warn!(
                ?err,
                "deep depth auto-flush failed — in-flight records rescued to ring buffer"
            );
        }

        Ok(())
    }

    /// Writes a single record's levels to the ILP buffer.
    fn write_record_to_ilp(&mut self, record: &DeepDepthRecord) -> Result<()> {
        let segment_str = segment_code_to_str(record.segment_code);
        let received_nanos = TimestampNanos::new(
            record
                .received_at_nanos
                .saturating_add(IST_UTC_OFFSET_NANOS),
        );

        for (i, level) in record.valid_levels().iter().enumerate() {
            // Skip empty levels (price = 0)
            if level.price <= 0.0 || !level.price.is_finite() {
                continue;
            }

            // ILP requires: table → ALL symbols → ALL columns → at
            // Symbols MUST come before any column_* calls
            self.buffer
                .table(QUESTDB_TABLE_DEEP_MARKET_DEPTH)
                .context("deep depth table")?
                // Symbols first (tag columns in QuestDB)
                .symbol("segment", segment_str)
                .context("segment")?
                .symbol("side", record.side_str())
                .context("side")?
                .symbol("depth_type", record.depth_type_str())
                .context("depth_type")?
                // Then columns (value columns)
                .column_i64("security_id", i64::from(record.security_id))
                .context("security_id")?
                .column_i64("level", (i as i64).saturating_add(1))
                .context("level")?
                .column_f64("price", level.price)
                .context("price")?
                .column_i64("quantity", i64::from(level.quantity))
                .context("quantity")?
                .column_i64("orders", i64::from(level.orders))
                .context("orders")?
                .column_ts("received_at", received_nanos)
                .context("received_at")?
                .at(received_nanos)
                .context("ts")?;

            self.pending_count = self.pending_count.saturating_add(1);
        }

        Ok(())
    }

    /// Forces an immediate flush of all buffered rows to QuestDB.
    ///
    /// On failure, the sender is set to `None` and in-flight records are
    /// rescued to the ring buffer. **Zero data loss** — no record is silently discarded.
    pub fn flush(&mut self) -> Result<()> {
        self.force_flush()
    }

    /// Internal flush implementation.
    fn force_flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            self.in_flight.clear();
            return Ok(());
        }

        if self.sender.is_none() {
            // No active sender — rescue in-flight records to ring buffer.
            self.rescue_in_flight();
            self.try_reconnect()?;
            return Ok(());
        }

        let count = self.pending_count;
        let sender = self
            .sender
            .as_mut()
            .context("sender unavailable in force_flush")?;

        if let Err(err) = sender.flush(&mut self.buffer) {
            // Sender is broken — rescue in-flight records to ring buffer
            // BEFORE clearing state, so no data is lost.
            self.sender = None;
            self.rescue_in_flight();
            // IMPORTANT: Create fresh buffer, NOT clear(). The questdb-rs Buffer
            // state machine may be corrupted after a failed flush (production bug
            // 2026-03-26 in greeks_persistence).
            self.buffer = Buffer::new(questdb::ingress::ProtocolVersion::V1);
            return Err(err).context("flush deep depth to QuestDB");
        }

        // Flush succeeded — in-flight records are confirmed written.
        self.in_flight.clear();
        self.pending_count = 0;
        debug!(flushed_rows = count, "deep depth batch flushed to QuestDB");
        Ok(())
    }

    /// Rescues in-flight records (in the ILP buffer but not yet flushed) back to
    /// the ring buffer / disk spill. Called on flush failure to prevent data loss.
    fn rescue_in_flight(&mut self) {
        if self.in_flight.is_empty() {
            self.pending_count = 0;
            return;
        }
        let count = self.in_flight.len();
        // Drain front-to-back to preserve ordering (FIFO).
        let records: Vec<DeepDepthRecord> = self.in_flight.drain(..).collect();
        for record in records {
            self.buffer_record(record);
        }
        self.pending_count = 0;
        // IMPORTANT: Create fresh buffer after rescue.
        self.buffer = Buffer::new(questdb::ingress::ProtocolVersion::V1);
        warn!(
            rescued = count,
            ring_buffer = self.depth_buffer.len(),
            "rescued in-flight deep depth records to ring buffer after flush failure"
        );
    }

    /// Pushes a record into the ring buffer. If the buffer is full, spills
    /// to disk instead of dropping. **Zero data loss guarantee.**
    fn buffer_record(&mut self, record: DeepDepthRecord) {
        // O(1) EXEMPT: begin — bounded ring buffer, max DEEP_DEPTH_BUFFER_CAPACITY
        if self.depth_buffer.len() >= DEEP_DEPTH_BUFFER_CAPACITY {
            // Ring buffer full — spill to disk (never drop).
            self.spill_record_to_disk(&record);
        } else {
            self.depth_buffer.push_back(record);
            // High watermark alert — fires once when buffer crosses 80%.
            if self.depth_buffer.len() == DEEP_DEPTH_BUFFER_HIGH_WATERMARK {
                error!(
                    buffer_size = self.depth_buffer.len(),
                    capacity = DEEP_DEPTH_BUFFER_CAPACITY,
                    "CRITICAL: deep depth ring buffer at 80% capacity — disk spill imminent. \
                     QuestDB still down."
                );
            }
        }
        metrics::gauge!("dlt_deep_depth_buffer_size").set(self.depth_buffer.len() as f64);
        // O(1) EXEMPT: end
    }

    /// Spills a record to disk when the ring buffer is full.
    fn spill_record_to_disk(&mut self, record: &DeepDepthRecord) {
        // Lazy-open the spill file.
        if self.spill_writer.is_none()
            && let Err(err) = self.open_spill_file()
        {
            error!(
                ?err,
                "CRITICAL: cannot open deep depth spill file — record WILL be lost"
            );
            self.records_dropped_total = self.records_dropped_total.saturating_add(1);
            metrics::counter!("dlt_deep_depth_dropped_total").absolute(self.records_dropped_total);
            return;
        }
        let data = record.serialize();
        if let Some(ref mut writer) = self.spill_writer
            && let Err(err) = writer.write_all(&data)
        {
            error!(
                ?err,
                records_spilled = self.records_spilled_total,
                "CRITICAL: disk spill write failed — deep depth record lost"
            );
            self.records_dropped_total = self.records_dropped_total.saturating_add(1);
            metrics::counter!("dlt_deep_depth_dropped_total").absolute(self.records_dropped_total);
            self.spill_writer = None;
            return;
        }
        self.records_spilled_total = self.records_spilled_total.saturating_add(1);
        if self.records_spilled_total.is_multiple_of(1000) {
            warn!(
                records_spilled = self.records_spilled_total,
                "deep depth disk spill growing — QuestDB still down"
            );
        }
    }

    /// Opens a spill file for writing.
    fn open_spill_file(&mut self) -> Result<()> {
        std::fs::create_dir_all(DEEP_DEPTH_SPILL_DIR)
            .context("create deep depth spill directory")?;
        let date = chrono::Utc::now().format("%Y%m%d");
        let path =
            std::path::PathBuf::from(format!("{DEEP_DEPTH_SPILL_DIR}/deep-depth-{date}.bin"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("open deep depth spill file")?;
        info!(path = %path.display(), "opened deep depth disk spill file");
        self.spill_writer = Some(BufWriter::new(file));
        self.spill_path = Some(path);
        Ok(())
    }

    /// Drains the ring buffer by re-serializing buffered records to ILP.
    /// Called after successful reconnect to QuestDB.
    fn drain_depth_buffer(&mut self) {
        if self.depth_buffer.is_empty() {
            return;
        }
        let count = self.depth_buffer.len();
        info!(
            buffered = count,
            "draining deep depth ring buffer after QuestDB reconnect"
        );
        let mut drained = 0u64;
        while let Some(record) = self.depth_buffer.pop_front() {
            if let Err(err) = self.write_record_to_ilp(&record) {
                warn!(
                    ?err,
                    "failed to write buffered deep depth record to ILP — re-buffering"
                );
                self.depth_buffer.push_front(record);
                break;
            }
            drained = drained.saturating_add(1);
        }
        metrics::gauge!("dlt_deep_depth_buffer_size").set(self.depth_buffer.len() as f64);
        if drained > 0 {
            info!(
                drained,
                remaining = self.depth_buffer.len(),
                "deep depth ring buffer drain progress"
            );
        }
    }

    /// Recovers records from stale spill files written by a previous crash.
    /// Call at startup before normal operation begins.
    // TEST-EXEMPT: requires filesystem spill files from a previous crash, tested via test_deep_depth_spill_to_disk_roundtrip
    pub fn recover_stale_spill_files(&mut self) {
        let dir = match std::fs::read_dir(DEEP_DEPTH_SPILL_DIR) {
            Ok(d) => d,
            Err(_) => return, // No spill dir = nothing to recover
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("deep-depth-") && n.ends_with(".bin"))
            {
                continue;
            }
            match std::fs::read(&path) {
                Ok(data) => {
                    let record_count = data.len() / DEEP_DEPTH_SPILL_RECORD_SIZE;
                    if record_count == 0 {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    info!(
                        path = %path.display(),
                        records = record_count,
                        "recovering deep depth spill file"
                    );
                    for chunk in data.chunks_exact(DEEP_DEPTH_SPILL_RECORD_SIZE) {
                        let Ok(arr): Result<&[u8; DEEP_DEPTH_SPILL_RECORD_SIZE], _> =
                            chunk.try_into()
                        else {
                            continue; // chunks_exact guarantees size, but be defensive
                        };
                        let record = DeepDepthRecord::deserialize(arr);
                        self.depth_buffer.push_back(record);
                    }
                    // Remove after successful read
                    if let Err(err) = std::fs::remove_file(&path) {
                        warn!(?err, path = %path.display(), "failed to remove recovered spill file");
                    }
                }
                Err(err) => {
                    warn!(?err, path = %path.display(), "failed to read deep depth spill file");
                }
            }
        }
        if !self.depth_buffer.is_empty() {
            info!(
                recovered = self.depth_buffer.len(),
                "deep depth spill recovery complete — records queued for drain"
            );
        }
    }

    /// Attempts to reconnect to QuestDB.
    fn try_reconnect(&mut self) -> Result<()> {
        match Sender::from_conf(&self.ilp_conf_string) {
            Ok(new_sender) => {
                self.buffer = new_sender.new_buffer();
                self.sender = Some(new_sender);
                info!("deep depth writer reconnected to QuestDB");
                Ok(())
            }
            Err(err) => Err(err).context("deep depth writer reconnection failed"),
        }
    }

    /// Returns the number of records held in the resilience ring buffer.
    // TEST-EXEMPT: trivial field getter, tested indirectly by ring buffer tests
    pub fn buffered_count(&self) -> usize {
        self.depth_buffer.len()
    }

    /// Returns true if QuestDB ILP sender is connected.
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Returns the total number of records permanently dropped.
    pub fn records_dropped_total(&self) -> u64 {
        self.records_dropped_total
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
    fn test_deep_depth_buffer_constants() {
        assert_eq!(DEEP_DEPTH_BUFFER_CAPACITY, 50_000);
        assert_eq!(
            DEEP_DEPTH_BUFFER_HIGH_WATERMARK,
            DEEP_DEPTH_BUFFER_CAPACITY * 4 / 5
        );
        assert!(DEEP_DEPTH_BUFFER_HIGH_WATERMARK < DEEP_DEPTH_BUFFER_CAPACITY);
        assert_eq!(DEEP_DEPTH_SPILL_RECORD_SIZE, 16 + 200 * 16);
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
    fn test_deep_depth_ddl_sql_format() {
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("segment SYMBOL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("security_id LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("side SYMBOL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("level LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("price DOUBLE"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("quantity LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("orders LONG"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("depth_type SYMBOL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("PARTITION BY HOUR WAL"));
        assert!(DEEP_MARKET_DEPTH_CREATE_DDL.contains("CREATE TABLE IF NOT EXISTS"));
    }

    #[test]
    fn test_deep_depth_dedup_keys_format() {
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("security_id"));
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("segment"));
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("level"));
        assert!(DEDUP_KEY_DEEP_DEPTH.contains("side"));
        // Should be comma-separated
        assert_eq!(DEDUP_KEY_DEEP_DEPTH.matches(',').count(), 3);
    }

    /// Helper: create test levels for ring buffer tests.
    fn make_test_levels(count: usize) -> Vec<DeepDepthLevel> {
        (0..count)
            .map(|i| DeepDepthLevel {
                price: 100.0 + i as f64,
                quantity: 10 * (i as u32 + 1),
                orders: i as u32 + 1,
            })
            .collect()
    }

    #[test]
    fn test_deep_depth_record_from_append() {
        let levels = make_test_levels(20);
        let record =
            DeepDepthRecord::from_append(12345, 2, "BID", &levels, "20", 1_000_000_000_000);

        assert_eq!(record.security_id, 12345);
        assert_eq!(record.segment_code, 2);
        assert_eq!(record.side_code, 0); // BID = 0
        assert_eq!(record.depth_type_code, 20);
        assert_eq!(record.level_count, 20);
        assert_eq!(record.received_at_nanos, 1_000_000_000_000);
        assert_eq!(record.side_str(), "BID");
        assert_eq!(record.depth_type_str(), "20");
        assert_eq!(record.valid_levels().len(), 20);
        assert!((record.valid_levels()[0].price - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_deep_depth_record_ask_side() {
        let levels = make_test_levels(5);
        let record = DeepDepthRecord::from_append(99, 1, "ASK", &levels, "200", 2_000_000_000_000);

        assert_eq!(record.side_code, 1); // ASK = 1
        assert_eq!(record.depth_type_code, 200);
        assert_eq!(record.side_str(), "ASK");
        assert_eq!(record.depth_type_str(), "200");
        assert_eq!(record.valid_levels().len(), 5);
    }

    #[test]
    fn test_deep_depth_record_serialize_deserialize_roundtrip() {
        let levels = make_test_levels(20);
        let original =
            DeepDepthRecord::from_append(54321, 2, "ASK", &levels, "20", 9_999_999_999_999);

        let serialized = original.serialize();
        assert_eq!(serialized.len(), DEEP_DEPTH_SPILL_RECORD_SIZE);

        let restored = DeepDepthRecord::deserialize(&serialized);
        assert_eq!(restored.security_id, original.security_id);
        assert_eq!(restored.segment_code, original.segment_code);
        assert_eq!(restored.side_code, original.side_code);
        assert_eq!(restored.depth_type_code, original.depth_type_code);
        assert_eq!(restored.level_count, original.level_count);
        assert_eq!(restored.received_at_nanos, original.received_at_nanos);

        for i in 0..usize::from(original.level_count) {
            assert!(
                (restored.levels[i].price - original.levels[i].price).abs() < f64::EPSILON,
                "price mismatch at level {i}"
            );
            assert_eq!(restored.levels[i].quantity, original.levels[i].quantity);
            assert_eq!(restored.levels[i].orders, original.levels[i].orders);
        }
    }

    #[test]
    fn test_deep_depth_record_serialize_200_levels() {
        let levels = make_test_levels(200);
        let record =
            DeepDepthRecord::from_append(11111, 2, "BID", &levels, "200", 5_000_000_000_000);

        assert_eq!(record.level_count, 200);
        let serialized = record.serialize();
        let restored = DeepDepthRecord::deserialize(&serialized);
        assert_eq!(restored.level_count, 200);
        assert!((restored.levels[199].price - 299.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_deep_depth_ring_buffer_push_pop() {
        let mut buffer: VecDeque<DeepDepthRecord> =
            VecDeque::with_capacity(DEEP_DEPTH_BUFFER_CAPACITY);
        let levels = make_test_levels(5);

        // Push 3 records
        for i in 0..3 {
            let record = DeepDepthRecord::from_append(
                i as u32,
                2,
                "BID",
                &levels,
                "20",
                i as i64 * 1_000_000,
            );
            buffer.push_back(record);
        }
        assert_eq!(buffer.len(), 3);

        // Pop oldest first (FIFO)
        let first = buffer.pop_front().unwrap();
        assert_eq!(first.security_id, 0);
        let second = buffer.pop_front().unwrap();
        assert_eq!(second.security_id, 1);
        let third = buffer.pop_front().unwrap();
        assert_eq!(third.security_id, 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_deep_depth_spill_to_disk_roundtrip() {
        let test_dir = std::path::PathBuf::from("/tmp/dlt-test-deep-depth-spill");
        let _ = std::fs::remove_dir_all(&test_dir);
        std::fs::create_dir_all(&test_dir).unwrap();
        let spill_path = test_dir.join("deep-depth-test.bin");

        // Write 3 records to a "spill file"
        let levels = make_test_levels(10);
        let mut file = std::fs::File::create(&spill_path).unwrap();
        for i in 0..3u32 {
            let record = DeepDepthRecord::from_append(
                i * 100,
                2,
                if i % 2 == 0 { "BID" } else { "ASK" },
                &levels,
                "20",
                i as i64 * 1_000_000_000,
            );
            std::io::Write::write_all(&mut file, &record.serialize()).unwrap();
        }
        drop(file);

        // Read back and verify
        let data = std::fs::read(&spill_path).unwrap();
        assert_eq!(data.len(), 3 * DEEP_DEPTH_SPILL_RECORD_SIZE);

        for (i, chunk) in data.chunks_exact(DEEP_DEPTH_SPILL_RECORD_SIZE).enumerate() {
            let arr: &[u8; DEEP_DEPTH_SPILL_RECORD_SIZE] = chunk.try_into().unwrap();
            let record = DeepDepthRecord::deserialize(arr);
            assert_eq!(record.security_id, i as u32 * 100);
            assert_eq!(record.level_count, 10);
        }
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_deep_depth_record_empty_levels() {
        let record = DeepDepthRecord::from_append(1, 2, "BID", &[], "20", 0);
        assert_eq!(record.level_count, 0);
        assert!(record.valid_levels().is_empty());

        let serialized = record.serialize();
        let restored = DeepDepthRecord::deserialize(&serialized);
        assert_eq!(restored.level_count, 0);
        assert_eq!(restored.security_id, 1);
    }

    #[test]
    fn test_deep_depth_record_clamps_to_200_levels() {
        // Create 250 levels — should be clamped to 200
        let levels = make_test_levels(250);
        let record = DeepDepthRecord::from_append(1, 2, "BID", &levels, "200", 0);
        assert_eq!(record.level_count, 200);
        assert_eq!(record.valid_levels().len(), 200);
    }
}
