//! Groww live-tick persistence — second feed (operator lock 2026-06-19 §32,
//! `groww-second-feed-scope-2026-06-19.md`). A SEPARATE QuestDB table holding
//! raw live ticks from the Groww feed. **This is NOT the Dhan `ticks` table**
//! and never shares a row with it — the `groww_*` namespace keeps the two feeds
//! fully isolated (the Dhan path is byte-identical whether Groww is on or off).
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS groww_live_ticks (
//!     segment             SYMBOL,
//!     security_id         LONG,    -- Groww exchange_token (composite key w/ segment)
//!     ts                  TIMESTAMP,-- producer-normalised IST nanos (ms-precise)
//!     ltp                 DOUBLE,  -- last traded price
//!     volume              LONG,    -- cumulative day volume
//!     exchange_ts_millis  LONG,    -- raw Groww millisecond timestamp, VERBATIM
//!     capture_seq         LONG     -- monotonic replay-stable dedup tiebreaker
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment, capture_seq);
//! ```
//!
//! ## Uniqueness / dedup (I-P1-11 + TICK-SEQ-01)
//!
//! DEDUP key `(ts, security_id, segment, capture_seq)`:
//! - `segment` is mandatory (I-P1-11 — `security_id` is reused across segments).
//! - `capture_seq` is the sub-`ts` tiebreaker AND the replay-idempotency key,
//!   mirroring the Dhan `ticks` design: two DISTINCT arrivals get DISTINCT
//!   `capture_seq` → BOTH kept (no loss, even when every value field is
//!   byte-identical — e.g. an index re-printing the same LTP); a true
//!   duplicate / replay reuses the SAME `capture_seq` → collapsed (idempotent).
//!   The producer stamps it monotonically at receipt and carries it unchanged
//!   through the durable file → ring → spill → DLQ → DB.
//!
//! ## Timestamp rule (`data-integrity.md`)
//!
//! `ts_ist_nanos` is supplied by the producer ALREADY normalised to IST nanos —
//! this persistence layer applies NO timezone offset (tz mapping is the
//! producer's job, discovered against the live Groww feed). Groww's raw
//! millisecond value is preserved VERBATIM in `exchange_ts_millis` (the
//! ms-precision advantage that motivated the second feed). `exchange_ts_millis`
//! is NOT a DEDUP key column.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::warn;

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per received Groww live tick.
pub const GROWW_LIVE_TICKS_TABLE: &str = "groww_live_ticks";

/// DEDUP UPSERT key — includes the designated timestamp `ts` (QuestDB requires
/// it), `segment` per I-P1-11, and `capture_seq` per TICK-SEQ-01. The
/// `dedup_segment_meta_guard` workspace test scans this constant; it MUST
/// mention `segment`.
pub const DEDUP_KEY_GROWW_LIVE_TICKS: &str = "ts, security_id, segment, capture_seq";

/// One Groww live tick ready for ILP write. `ltp` is `f64` (the Groww SDK
/// emits a native float — no `f32_to_f64_clean` widening concern, which is
/// Dhan-WebSocket-`f32`-specific per `data-integrity.md`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowwLiveTickRow {
    /// Designated timestamp: producer-normalised IST nanoseconds (ms-precise).
    /// NO offset is applied here — the producer already converted to IST.
    pub ts_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11) — Groww exchange_token, widened to `i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). `&'static` segment string
    /// (`NSE_EQ` / `NSE_FNO` / `IDX_I` / …) for the `symbol` column.
    pub segment: &'static str,
    /// Last traded price.
    pub ltp: f64,
    /// Cumulative day volume.
    pub volume: i64,
    /// Raw Groww millisecond timestamp, stored VERBATIM. NOT a DEDUP key column.
    pub exchange_ts_millis: i64,
    /// Monotonic, replay-stable dedup tiebreaker (TICK-SEQ-01).
    pub capture_seq: i64,
}

/// The idempotent `CREATE TABLE` DDL for `groww_live_ticks`. Pure (testable
/// without QuestDB).
#[must_use]
pub fn groww_live_ticks_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {GROWW_LIVE_TICKS_TABLE} (\
            segment             SYMBOL, \
            security_id         LONG, \
            ts                  TIMESTAMP, \
            ltp                 DOUBLE, \
            volume              LONG, \
            exchange_ts_millis  LONG, \
            capture_seq         LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_GROWW_LIVE_TICKS});"
    )
}

/// Create the `groww_live_ticks` table if absent (idempotent, schema-self-heal
/// pattern). Failures log at `error!` (Telegram-routable) but do NOT block the
/// caller. Only invoked when the Groww feed is enabled.
// Boot wiring lands with the Groww producer PR (the consumer that writes this table).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via groww_live_ticks_create_ddl tests).
pub async fn ensure_groww_live_ticks_table(questdb_config: &QuestDbConfig) {
    use std::time::Duration;
    use tracing::error;

    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                ?err,
                "groww_live_ticks: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = groww_live_ticks_create_ddl();
    match client
        .get(&base_url)
        .query(&[("query", ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "groww_live_ticks: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(?err, "groww_live_ticks: CREATE TABLE request failed"),
    }
}

/// Lazy-connect ILP writer for the `groww_live_ticks` table. Mirrors
/// `PrevDayOhlcvWriter`: if QuestDB is unreachable at construction the writer
/// still builds (`sender = None`); `append_row` fills the local buffer and
/// `flush` returns `Err` until a reconnect lands. The durable floor for Groww
/// ticks is the producer's capture-at-receipt file (lock §32), so a flush
/// failure here is recoverable, not data loss.
pub struct GrowwLiveTickWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl GrowwLiveTickWriter {
    /// Production constructor — connects via ILP TCP, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); the
    // disconnected/append/flush paths are covered via for_test().
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "groww_live_ticks writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// `true` when a live ILP sender is held.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by test_flush_when_disconnected_errors.
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Rows buffered since the last flush.
    #[must_use]
    pub const fn pending(&self) -> usize {
        self.pending
    }

    /// Append one Groww tick row to the ILP buffer. O(1), zero alloc beyond the
    /// buffer's internal growth.
    pub fn append_row(&mut self, row: &GrowwLiveTickRow) -> Result<()> {
        self.buffer
            .table(GROWW_LIVE_TICKS_TABLE)
            .with_context(|| "groww_live_ticks append: table() failed")?
            .symbol("segment", row.segment)
            .with_context(|| "groww_live_ticks append: symbol(segment) failed")?
            .column_i64("security_id", row.security_id)
            .with_context(|| "groww_live_ticks append: column_i64(security_id) failed")?
            .column_f64("ltp", row.ltp)
            .with_context(|| "groww_live_ticks append: column_f64(ltp) failed")?
            .column_i64("volume", row.volume)
            .with_context(|| "groww_live_ticks append: column_i64(volume) failed")?
            .column_i64("exchange_ts_millis", row.exchange_ts_millis)
            .with_context(|| "groww_live_ticks append: column_i64(exchange_ts_millis) failed")?
            .column_i64("capture_seq", row.capture_seq)
            .with_context(|| "groww_live_ticks append: column_i64(capture_seq) failed")?
            .at(TimestampNanos::new(row.ts_ist_nanos))
            .with_context(|| "groww_live_ticks append: at(ts) failed")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Bytes currently buffered (observability + tests).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests.
    pub fn buffer_byte_count(&self) -> usize {
        self.buffer.len()
    }

    /// Raw buffered bytes (tests assert table/segment/value serialisation).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests.
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Flush the buffer to QuestDB. `Err` if disconnected (caller logs + the
    /// producer's capture file remains the durable record; no data loss).
    pub fn flush(&mut self) -> Result<()> {
        let sender = self
            .sender
            .as_mut()
            .context("groww_live_ticks flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("groww_live_ticks flush: ILP flush failed")?;
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> GrowwLiveTickRow {
        GrowwLiveTickRow {
            ts_ist_nanos: 1_780_000_000_123_000_000,
            security_id: 1_333,
            segment: "NSE_EQ",
            ltp: 2_847.55,
            volume: 1_234_567,
            exchange_ts_millis: 1_780_000_000_123,
            capture_seq: 42,
        }
    }

    #[test]
    fn test_dedup_key_includes_segment_and_capture_seq() {
        // I-P1-11 + dedup_segment_meta_guard + TICK-SEQ-01 contracts.
        assert!(DEDUP_KEY_GROWW_LIVE_TICKS.contains("segment"));
        assert!(DEDUP_KEY_GROWW_LIVE_TICKS.contains("security_id"));
        assert!(DEDUP_KEY_GROWW_LIVE_TICKS.contains("ts"));
        assert!(DEDUP_KEY_GROWW_LIVE_TICKS.contains("capture_seq"));
        // exchange_ts_millis is mutable raw data, NOT identity — must NOT be a key.
        assert!(!DEDUP_KEY_GROWW_LIVE_TICKS.contains("exchange_ts_millis"));
    }

    #[test]
    fn test_groww_live_ticks_create_ddl_has_all_columns_and_dedup() {
        let ddl = groww_live_ticks_create_ddl();
        for needle in [
            "groww_live_ticks",
            "segment             SYMBOL",
            "security_id         LONG",
            "ts                  TIMESTAMP",
            "ltp                 DOUBLE",
            "volume              LONG",
            "exchange_ts_millis  LONG",
            "capture_seq         LONG",
            "DEDUP UPSERT KEYS(ts, security_id, segment, capture_seq)",
            "PARTITION BY DAY",
        ] {
            assert!(ddl.contains(needle), "DDL missing: {needle}\n{ddl}");
        }
    }

    #[test]
    fn test_table_name_is_groww_namespaced_not_dhan_ticks() {
        // Isolation contract (lock §32): never the Dhan `ticks` table.
        assert_eq!(GROWW_LIVE_TICKS_TABLE, "groww_live_ticks");
        assert_ne!(GROWW_LIVE_TICKS_TABLE, "ticks");
        assert!(GROWW_LIVE_TICKS_TABLE.starts_with("groww_"));
    }

    #[test]
    fn test_for_test_writer_is_disconnected_and_empty() {
        let w = GrowwLiveTickWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_append_row_serialises_table_segment_and_ms_timestamp() {
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("groww_live_ticks"), "table name on wire");
        assert!(text.contains("NSE_EQ"), "segment on wire");
        // The ms-precision value is preserved verbatim on the wire.
        assert!(
            text.contains("exchange_ts_millis=1780000000123"),
            "raw ms timestamp on wire: {text}"
        );
        assert!(text.contains("capture_seq=42"), "capture_seq on wire");
    }

    #[test]
    fn test_distinct_capture_seq_same_values_both_buffered() {
        // Zero-loss tiebreaker: two same-value ticks with distinct capture_seq
        // both reach the wire (the index `45→75→45` loss class — TICK-SEQ-01).
        let mut w = GrowwLiveTickWriter::for_test();
        let mut a = sample_row();
        a.capture_seq = 100;
        let mut b = sample_row();
        b.capture_seq = 101;
        w.append_row(&a).expect("a");
        w.append_row(&b).expect("b");
        assert_eq!(w.pending(), 2);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("capture_seq=100"));
        assert!(text.contains("capture_seq=101"));
    }

    #[test]
    fn test_flush_when_disconnected_errors_not_panics() {
        let mut w = GrowwLiveTickWriter::for_test();
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }

    #[test]
    fn test_two_appends_increment_pending() {
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("a1");
        w.append_row(&sample_row()).expect("a2");
        assert_eq!(w.pending(), 2);
    }
}
