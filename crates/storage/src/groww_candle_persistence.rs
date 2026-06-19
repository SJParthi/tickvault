//! Groww 1-minute candle persistence — second feed (operator lock §32,
//! `groww-second-feed-scope-2026-06-19.md`). A SEPARATE QuestDB table holding
//! the 1-minute OHLCV candles aggregated from the Groww live feed. **This is NOT
//! the Dhan `candles_1m` table** — the `groww_*` namespace keeps the two feeds
//! fully isolated, so the live-1m-vs-backtest-1m parity check (the goal of the
//! second feed) compares Groww-against-Groww without touching Dhan data.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS groww_candles_1m (
//!     feed         SYMBOL,   -- broker source provenance label ("groww")
//!     segment      SYMBOL,
//!     security_id  LONG,     -- Groww exchange_token (composite key w/ segment)
//!     ts           TIMESTAMP,-- minute-aligned IST nanos
//!     open         DOUBLE,
//!     high         DOUBLE,
//!     low          DOUBLE,
//!     close        DOUBLE,
//!     volume       LONG,
//!     tick_count   LONG      -- # ticks folded into this candle (observability)
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment);
//! ```
//!
//! ## Uniqueness / dedup (I-P1-11)
//!
//! DEDUP key `(ts, security_id, segment)` — one candle per (minute, instrument).
//! `segment` is mandatory (I-P1-11). NO `capture_seq` (unlike ticks): a sealed
//! 1-minute candle is uniquely identified by its minute boundary, so re-sealing
//! the same minute UPSERTs in place (idempotent). `feed` is a provenance LABEL,
//! NOT a key (the table is groww-only; feeds are physically separated).
//!
//! ## Timestamp rule (`data-integrity.md`)
//!
//! `ts_ist_nanos` is the minute-aligned IST designated timestamp supplied by the
//! aggregator (already IST — NO offset applied here).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::warn;

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per (minute, instrument) Groww candle.
pub const GROWW_CANDLES_1M_TABLE: &str = "groww_candles_1m";

/// DEDUP UPSERT key — `ts` (designated, QuestDB-required) + `segment` per
/// I-P1-11. The `dedup_segment_meta_guard` workspace test scans this constant;
/// it MUST mention `segment`.
pub const DEDUP_KEY_GROWW_CANDLES_1M: &str = "ts, security_id, segment";

/// One Groww 1-minute candle ready for ILP write. All prices `f64` (the Groww
/// SDK emits native floats — no `f32_to_f64_clean` widening concern).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowwCandle1mRow {
    /// Designated timestamp: minute-aligned IST nanoseconds. NO offset here.
    pub ts_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11) — Groww exchange_token, widened to `i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). `&'static` segment string for the
    /// `symbol` column.
    pub segment: &'static str,
    /// Broker-source provenance label (e.g. `"groww"`). NON-key.
    pub feed: &'static str,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    /// Number of ticks folded into this candle (observability; NOT a key).
    pub tick_count: i64,
}

/// The idempotent `CREATE TABLE` DDL for `groww_candles_1m`. Pure (testable
/// without QuestDB).
#[must_use]
pub fn groww_candles_1m_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {GROWW_CANDLES_1M_TABLE} (\
            feed         SYMBOL, \
            segment      SYMBOL, \
            security_id  LONG, \
            ts           TIMESTAMP, \
            open         DOUBLE, \
            high         DOUBLE, \
            low          DOUBLE, \
            close        DOUBLE, \
            volume       LONG, \
            tick_count   LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_GROWW_CANDLES_1M});"
    )
}

/// Create the `groww_candles_1m` table if absent (idempotent, schema-self-heal
/// pattern). Failures log at `error!` (Telegram-routable) but do NOT block the
/// caller. Only invoked when the Groww feed is enabled.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via groww_candles_1m_create_ddl tests).
pub async fn ensure_groww_candles_1m_table(questdb_config: &QuestDbConfig) {
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
                "groww_candles_1m: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = groww_candles_1m_create_ddl();
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
                "groww_candles_1m: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(?err, "groww_candles_1m: CREATE TABLE request failed"),
    }
}

/// Lazy-connect ILP writer for the `groww_candles_1m` table. Mirrors
/// `PrevDayOhlcvWriter`: if QuestDB is unreachable at construction the writer
/// still builds (`sender = None`); `append_row` fills the local buffer and
/// `flush` returns `Err` until a reconnect lands.
pub struct GrowwCandle1mWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl GrowwCandle1mWriter {
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
                    "groww_candles_1m writer: QuestDB unreachable — buffering locally"
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

    /// Append one Groww 1m candle row to the ILP buffer. O(1), zero alloc beyond
    /// the buffer's internal growth.
    pub fn append_row(&mut self, row: &GrowwCandle1mRow) -> Result<()> {
        self.buffer
            .table(GROWW_CANDLES_1M_TABLE)
            .with_context(|| "groww_candles_1m append: table() failed")?
            .symbol("feed", row.feed)
            .with_context(|| "groww_candles_1m append: symbol(feed) failed")?
            .symbol("segment", row.segment)
            .with_context(|| "groww_candles_1m append: symbol(segment) failed")?
            .column_i64("security_id", row.security_id)
            .with_context(|| "groww_candles_1m append: column_i64(security_id) failed")?
            .column_f64("open", row.open)
            .with_context(|| "groww_candles_1m append: column_f64(open) failed")?
            .column_f64("high", row.high)
            .with_context(|| "groww_candles_1m append: column_f64(high) failed")?
            .column_f64("low", row.low)
            .with_context(|| "groww_candles_1m append: column_f64(low) failed")?
            .column_f64("close", row.close)
            .with_context(|| "groww_candles_1m append: column_f64(close) failed")?
            .column_i64("volume", row.volume)
            .with_context(|| "groww_candles_1m append: column_i64(volume) failed")?
            .column_i64("tick_count", row.tick_count)
            .with_context(|| "groww_candles_1m append: column_i64(tick_count) failed")?
            .at(TimestampNanos::new(row.ts_ist_nanos))
            .with_context(|| "groww_candles_1m append: at(ts) failed")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Raw buffered bytes (tests assert table/feed/value serialisation).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests.
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Bytes currently buffered (observability + tests).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests.
    pub fn buffer_byte_count(&self) -> usize {
        self.buffer.len()
    }

    /// Flush the buffer to QuestDB. `Err` if disconnected.
    pub fn flush(&mut self) -> Result<()> {
        let sender = self
            .sender
            .as_mut()
            .context("groww_candles_1m flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("groww_candles_1m flush: ILP flush failed")?;
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> GrowwCandle1mRow {
        GrowwCandle1mRow {
            ts_ist_nanos: 1_780_000_020_000_000_000,
            security_id: 1_333,
            segment: "NSE_EQ",
            feed: "groww",
            open: 2_840.0,
            high: 2_850.5,
            low: 2_838.0,
            close: 2_847.55,
            volume: 123_456,
            tick_count: 87,
        }
    }

    #[test]
    fn test_dedup_key_includes_segment_excludes_feed() {
        // I-P1-11 + dedup_segment_meta_guard; feed is a LABEL, not a key.
        assert!(DEDUP_KEY_GROWW_CANDLES_1M.contains("segment"));
        assert!(DEDUP_KEY_GROWW_CANDLES_1M.contains("security_id"));
        assert!(DEDUP_KEY_GROWW_CANDLES_1M.contains("ts"));
        assert!(!DEDUP_KEY_GROWW_CANDLES_1M.contains("feed"));
        assert!(!DEDUP_KEY_GROWW_CANDLES_1M.contains("capture_seq"));
    }

    #[test]
    fn test_groww_candles_1m_create_ddl_has_all_columns_and_dedup() {
        let ddl = groww_candles_1m_create_ddl();
        for needle in [
            "groww_candles_1m",
            "feed         SYMBOL",
            "segment      SYMBOL",
            "security_id  LONG",
            "ts           TIMESTAMP",
            "open         DOUBLE",
            "high         DOUBLE",
            "low          DOUBLE",
            "close        DOUBLE",
            "volume       LONG",
            "tick_count   LONG",
            "DEDUP UPSERT KEYS(ts, security_id, segment)",
            "PARTITION BY DAY",
        ] {
            assert!(ddl.contains(needle), "DDL missing: {needle}\n{ddl}");
        }
    }

    #[test]
    fn test_table_name_is_groww_namespaced_not_dhan_candles() {
        // Isolation contract (lock §32): never the Dhan `candles_1m` table.
        assert_eq!(GROWW_CANDLES_1M_TABLE, "groww_candles_1m");
        assert_ne!(GROWW_CANDLES_1M_TABLE, "candles_1m");
        assert!(GROWW_CANDLES_1M_TABLE.starts_with("groww_"));
    }

    #[test]
    fn test_for_test_writer_is_disconnected_and_empty() {
        let w = GrowwCandle1mWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_append_row_serialises_table_feed_and_ohlc() {
        let mut w = GrowwCandle1mWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(text.contains("groww_candles_1m"), "table name on wire");
        assert!(text.contains("feed=groww"), "feed provenance on wire");
        assert!(text.contains("NSE_EQ"), "segment on wire");
        assert!(text.contains("tick_count=87"), "tick_count on wire");
    }

    #[test]
    fn test_flush_when_disconnected_errors_not_panics() {
        let mut w = GrowwCandle1mWriter::for_test();
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }

    #[test]
    fn test_two_appends_increment_pending() {
        let mut w = GrowwCandle1mWriter::for_test();
        w.append_row(&sample_row()).expect("a1");
        w.append_row(&sample_row()).expect("a2");
        assert_eq!(w.pending(), 2);
    }
}
