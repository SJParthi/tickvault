//! Previous-day OHLCV persistence — operator directive 2026-06-01
//! (`live-feed-purity.md` rule 9). A SEPARATE QuestDB table holding
//! yesterday's single daily candle (O/H/L/C/V) per subscribed instrument,
//! fetched ONCE at boot via Dhan REST `/v2/charts/historical` (daily).
//!
//! **This is NOT the `ticks` table** and NOT the deleted 90-day historical
//! chain. One row per (instrument, prev-trading-day, feed). DEDUP UPSERT KEYS
//! `(ts, security_id, segment, feed)` per I-P1-11 + per-feed identity
//! (2026-06-23) → idempotent re-runs collapse to one row; cross-segment
//! same-`security_id` instruments stay distinct, and each feed (dhan/groww)
//! keeps its own prev-day candle.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS prev_day_ohlcv (
//!     feed         SYMBOL,      -- broker source (dhan/groww) — per-feed identity
//!     segment      SYMBOL,
//!     security_id  LONG,
//!     ts           TIMESTAMP,   -- prev trading-day IST midnight (nanos)
//!     open         DOUBLE,
//!     high         DOUBLE,
//!     low          DOUBLE,
//!     close        DOUBLE,
//!     volume       LONG,
//!     oi           LONG         -- prev-day open interest (F&O only; 0 for IDX_I/equity)
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment, feed);
//! ```
//!
//! ## `oi` column (operator directive 2026-06-02 — 1d historical-only)
//!
//! When the live aggregator stopped sealing the `D1` (1-day) timeframe (1d is
//! now historical-only), the prev-OI cache lost its `candles_1d` source. This
//! table is now the prev-OI source, so it carries the previous-day open
//! interest. Dhan REST `/v2/charts/historical` returns `open_interest` as a
//! columnar array (all-zero for equities/indices where OI is meaningless).
//! `oi` is NOT a DEDUP key column — only `(ts, security_id, segment, feed)` are.
//!
//! ## Timestamp rule (`data-integrity.md`)
//!
//! Dhan REST historical timestamps are **UNIX UTC epoch seconds**, so the
//! IST-displayed designated timestamp adds `+IST_UTC_OFFSET` (19800 s) —
//! the OPPOSITE of WebSocket LTT (already IST). See
//! [`prev_day_ist_nanos_from_utc_secs`].

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;

/// QuestDB table name. One row per (instrument, prev-trading-day).
pub const PREV_DAY_OHLCV_TABLE: &str = "prev_day_ohlcv";

/// DEDUP UPSERT key — includes the designated timestamp `ts` (QuestDB
/// requires it), `segment` per I-P1-11, AND `feed` per the per-feed
/// identity model (operator 2026-06-23): each feed (dhan/groww) has its
/// OWN previous-day OHLCV (their daily candles can differ), so a Dhan and
/// a Groww row for the same `(ts, security_id, segment)` are BOTH kept,
/// never collapsed. The `dedup_segment_meta_guard` workspace test scans
/// this constant; it MUST mention `segment`.
pub const DEDUP_KEY_PREV_DAY_OHLCV: &str = "ts, security_id, segment, feed";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One previous-day daily candle ready for ILP write. All prices are
/// `f64` (Dhan REST returns f64 natively — no `f32_to_f64_clean` needed
/// per `data-integrity.md`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrevDayOhlcvRow {
    /// Broker feed source (`dhan` / `groww`). Composite-key part (per-feed
    /// identity, 2026-06-23) — each feed's prev-day candle is a distinct row.
    /// `&'static str` from `tickvault_common::feed::Feed::as_str()` — zero-alloc.
    pub feed: &'static str,
    /// Designated timestamp: prev trading-day IST midnight, in nanoseconds.
    pub ts_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11). Widened to `i64` for `column_i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). `&'static` segment string
    /// (`IDX_I` / `NSE_EQ` / …) for the `symbol` column.
    pub segment: &'static str,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    /// Previous-day open interest. Meaningful for F&O only; Dhan returns
    /// all-zero for IDX_I / equity. Sources the prev-OI cache (1d
    /// historical-only directive 2026-06-02). NOT a DEDUP key column.
    pub oi: i64,
}

/// Convert a Dhan REST historical candle UTC-epoch-second timestamp into
/// the IST-midnight designated-timestamp nanoseconds the table stores.
///
/// `data-integrity.md`: historical REST timestamps are UTC epoch seconds,
/// so we add `+IST_UTC_OFFSET_SECONDS` (19800) — the OPPOSITE of the
/// WebSocket LTT rule (which is already IST and must NEVER get the offset).
///
/// O(1) — 1 add + 1 multiply. Saturating to avoid overflow panics.
#[must_use]
pub fn prev_day_ist_nanos_from_utc_secs(utc_epoch_secs: i64) -> i64 {
    utc_epoch_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .saturating_mul(1_000_000_000)
}

/// The idempotent `CREATE TABLE` DDL for `prev_day_ohlcv`. Pure (testable
/// without QuestDB).
#[must_use]
pub fn prev_day_ohlcv_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {PREV_DAY_OHLCV_TABLE} (\
            feed         SYMBOL, \
            segment      SYMBOL, \
            security_id  LONG, \
            ts           TIMESTAMP, \
            open         DOUBLE, \
            high         DOUBLE, \
            low          DOUBLE, \
            close        DOUBLE, \
            volume       LONG, \
            oi           LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_PREV_DAY_OHLCV});"
    )
}

/// Create the `prev_day_ohlcv` table if absent (idempotent, schema-self-heal
/// pattern). Failures log at `error!` (Telegram-routable) but do NOT block
/// boot — the fetcher simply can't persist and reports it.
// Requires a live QuestDB; the DDL string is unit-tested via the
// `prev_day_ohlcv_create_ddl` tests. Boot wiring lives in
// crates/app/src/main.rs (called by run_prev_day_ohlcv_fetch).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested).
pub async fn ensure_prev_day_ohlcv_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                ?err,
                "prev_day_ohlcv: HTTP client build failed — table not ensured"
            );
            return;
        }
    };
    let ddl = prev_day_ohlcv_create_ddl();
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
                "prev_day_ohlcv: CREATE TABLE returned non-2xx");
        }
        Err(err) => error!(?err, "prev_day_ohlcv: CREATE TABLE request failed"),
    }

    // Schema self-heal (observability-architecture.md): a table created by an
    // earlier build (before the 2026-06-02 1d-historical-only `oi` column)
    // auto-migrates here. QuestDB ignores ADD COLUMN that already exists, so
    // running every boot is free.
    match client
        .get(&base_url)
        .query(&[("query", prev_day_ohlcv_alter_add_oi_ddl())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "prev_day_ohlcv: ALTER ADD COLUMN oi returned non-2xx");
        }
        Err(err) => error!(?err, "prev_day_ohlcv: ALTER ADD COLUMN oi request failed"),
    }

    // Feed-provenance label (operator 2026-06-19): future-ready broker source
    // column (dhan/groww). Additive + idempotent; NON-key. Self-heal so existing
    // live prev_day_ohlcv tables gain it. Free on every boot.
    match client
        .get(&base_url)
        .query(&[("query", prev_day_ohlcv_alter_add_feed_ddl())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "prev_day_ohlcv: ALTER ADD COLUMN feed returned non-2xx");
        }
        Err(err) => error!(?err, "prev_day_ohlcv: ALTER ADD COLUMN feed request failed"),
    }

    // Per-feed identity (operator 2026-06-23): re-apply the DEDUP key so it
    // includes `feed` on tables created before this directive. Runs AFTER the
    // `feed` column ALTER above so the key column exists. Idempotent; never
    // drops the table (SEBI). Mirrors the ticks `DEDUP ENABLE` self-heal.
    match client
        .get(&base_url)
        .query(&[("query", prev_day_ohlcv_dedup_reenable_ddl().as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(%status, body = %body.chars().take(200).collect::<String>(),
                "prev_day_ohlcv: DEDUP ENABLE UPSERT KEYS returned non-2xx");
        }
        Err(err) => error!(
            ?err,
            "prev_day_ohlcv: DEDUP ENABLE UPSERT KEYS request failed"
        ),
    }
}

/// Idempotent `ALTER TABLE ADD COLUMN IF NOT EXISTS oi LONG`. Pure (testable
/// without QuestDB). Schema self-heal for tables created before the
/// 2026-06-02 1d-historical-only directive added the `oi` column.
#[must_use]
pub fn prev_day_ohlcv_alter_add_oi_ddl() -> &'static str {
    "ALTER TABLE prev_day_ohlcv ADD COLUMN IF NOT EXISTS oi LONG;"
}

/// Idempotent `ALTER TABLE ADD COLUMN IF NOT EXISTS feed SYMBOL`. Pure (testable
/// without QuestDB). Schema self-heal — the feed-provenance label (broker
/// source) for tables created before the 2026-06-19 feed-column directive.
#[must_use]
pub fn prev_day_ohlcv_alter_add_feed_ddl() -> &'static str {
    "ALTER TABLE prev_day_ohlcv ADD COLUMN IF NOT EXISTS feed SYMBOL;"
}

/// Idempotent `ALTER TABLE … DEDUP ENABLE UPSERT KEYS(…)` re-applying the
/// per-feed key on EXISTING tables. A fresh `CREATE TABLE` already sets the
/// key (via [`prev_day_ohlcv_create_ddl`]), but a table created before the
/// 2026-06-23 per-feed-identity directive keeps its old `(ts, security_id,
/// segment)` key until this re-enable runs. Mirrors the `ticks` self-heal
/// (`ensure_tick_table_dedup_keys`). Idempotent — re-enabling is a no-op.
/// Never drops the table (SEBI retention). Pure (testable without QuestDB).
#[must_use]
pub fn prev_day_ohlcv_dedup_reenable_ddl() -> String {
    format!(
        "ALTER TABLE {PREV_DAY_OHLCV_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_PREV_DAY_OHLCV});"
    )
}

/// Lazy-connect ILP writer for the `prev_day_ohlcv` table. Mirrors
/// `ShadowCandleWriter`: if QuestDB is unreachable at construction the
/// writer still builds (`sender = None`); `append_row` fills the local
/// buffer and `flush` returns `Err` until a reconnect lands.
pub struct PrevDayOhlcvWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl PrevDayOhlcvWriter {
    /// Production constructor — connects via ILP TCP, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); the disconnected/append/flush paths are covered via for_test().
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
                    "prev_day_ohlcv writer: QuestDB unreachable — buffering locally"
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

    /// Append one prev-day candle row to the ILP buffer. O(1), zero alloc
    /// beyond the buffer's internal growth.
    pub fn append_row(&mut self, row: &PrevDayOhlcvRow) -> Result<()> {
        self.buffer
            .table(PREV_DAY_OHLCV_TABLE)
            .with_context(|| "prev_day_ohlcv append: table() failed")?
            .symbol("feed", row.feed)
            .with_context(|| "prev_day_ohlcv append: symbol(feed) failed")?
            .symbol("segment", row.segment)
            .with_context(|| "prev_day_ohlcv append: symbol(segment) failed")?
            .column_i64("security_id", row.security_id)
            .with_context(|| "prev_day_ohlcv append: column_i64(security_id) failed")?
            .column_f64("open", row.open)
            .with_context(|| "prev_day_ohlcv append: column_f64(open) failed")?
            .column_f64("high", row.high)
            .with_context(|| "prev_day_ohlcv append: column_f64(high) failed")?
            .column_f64("low", row.low)
            .with_context(|| "prev_day_ohlcv append: column_f64(low) failed")?
            .column_f64("close", row.close)
            .with_context(|| "prev_day_ohlcv append: column_f64(close) failed")?
            .column_i64("volume", row.volume)
            .with_context(|| "prev_day_ohlcv append: column_i64(volume) failed")?
            .column_i64("oi", row.oi)
            .with_context(|| "prev_day_ohlcv append: column_i64(oi) failed")?
            .at(TimestampNanos::new(row.ts_ist_nanos))
            .with_context(|| "prev_day_ohlcv append: at(ts) failed")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Bytes currently buffered (observability + tests).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by test_append_row_fills_buffer_with_table_and_segment.
    pub fn buffer_byte_count(&self) -> usize {
        self.buffer.len()
    }

    /// Raw buffered bytes (tests assert table/segment/value serialisation).
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by test_append_row_fills_buffer_with_table_and_segment.
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Flush the buffer to QuestDB. `Err` if disconnected (caller logs +
    /// the cold-path fetch reports the failure; boot is unaffected).
    pub fn flush(&mut self) -> Result<()> {
        let sender = self
            .sender
            .as_mut()
            .context("prev_day_ohlcv flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("prev_day_ohlcv flush: ILP flush failed")?;
        self.pending = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_key_includes_segment() {
        // I-P1-11 + dedup_segment_meta_guard contract.
        assert!(DEDUP_KEY_PREV_DAY_OHLCV.contains("segment"));
        assert!(DEDUP_KEY_PREV_DAY_OHLCV.contains("security_id"));
        assert!(DEDUP_KEY_PREV_DAY_OHLCV.contains("ts"));
    }

    #[test]
    fn test_dedup_key_includes_feed() {
        // Per-feed identity (2026-06-23): each feed's prev-day candle is a
        // distinct row, so `feed` MUST be in the DEDUP key.
        assert!(
            DEDUP_KEY_PREV_DAY_OHLCV.contains("feed"),
            "prev_day_ohlcv DEDUP key must include feed: {DEDUP_KEY_PREV_DAY_OHLCV}"
        );
        assert_eq!(DEDUP_KEY_PREV_DAY_OHLCV, "ts, security_id, segment, feed");
    }

    #[test]
    fn test_prev_day_ohlcv_dedup_reenable_ddl_includes_feed_key() {
        let ddl = prev_day_ohlcv_dedup_reenable_ddl();
        assert!(ddl.contains("ALTER TABLE prev_day_ohlcv"));
        assert!(ddl.contains("DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, feed)"));
    }

    #[test]
    fn test_prev_day_ohlcv_create_ddl_has_all_columns() {
        let ddl = prev_day_ohlcv_create_ddl();
        for needle in [
            "prev_day_ohlcv",
            "feed         SYMBOL",
            "segment      SYMBOL",
            "security_id  LONG",
            "ts           TIMESTAMP",
            "open         DOUBLE",
            "high         DOUBLE",
            "low          DOUBLE",
            "close        DOUBLE",
            "volume       LONG",
            "oi           LONG",
            "DEDUP UPSERT KEYS(ts, security_id, segment, feed)",
            "PARTITION BY DAY",
        ] {
            assert!(ddl.contains(needle), "DDL missing: {needle}\n{ddl}");
        }
        // `oi` must NOT be part of the DEDUP key (it is mutable data, not identity).
        assert!(
            !ddl.contains("segment, feed, oi") && !ddl.contains("oi)"),
            "oi must not be a DEDUP key column"
        );
    }

    #[test]
    fn test_prev_day_ohlcv_alter_add_oi_ddl_is_idempotent_add_column() {
        let alter = prev_day_ohlcv_alter_add_oi_ddl();
        assert!(alter.contains("ALTER TABLE prev_day_ohlcv"));
        assert!(alter.contains("ADD COLUMN IF NOT EXISTS oi LONG"));
    }

    #[test]
    fn test_prev_day_ohlcv_alter_add_feed_ddl_is_idempotent_add_column() {
        let alter = prev_day_ohlcv_alter_add_feed_ddl();
        assert!(alter.contains("ALTER TABLE prev_day_ohlcv"));
        assert!(alter.contains("ADD COLUMN IF NOT EXISTS feed SYMBOL"));
    }

    #[test]
    fn test_prev_day_ist_nanos_from_utc_secs_adds_offset() {
        // Historical REST = UTC epoch secs → +19800 for IST, then ×1e9.
        // 2026-05-29 00:00:00 UTC = 1_780_012_800 (example); IST = +19800.
        let utc = 1_780_012_800_i64;
        let got = prev_day_ist_nanos_from_utc_secs(utc);
        assert_eq!(got, (utc + 19_800) * 1_000_000_000);
        // Saturating guard — no panic at extremes.
        assert_eq!(prev_day_ist_nanos_from_utc_secs(i64::MAX), i64::MAX);
    }

    #[test]
    fn test_for_test_writer_is_disconnected_and_empty() {
        let w = PrevDayOhlcvWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_append_row_fills_buffer_with_table_and_segment() {
        let mut w = PrevDayOhlcvWriter::for_test();
        let row = PrevDayOhlcvRow {
            feed: "dhan",
            ts_ist_nanos: (1_780_012_800_i64 + 19_800) * 1_000_000_000,
            security_id: 13,
            segment: "IDX_I",
            open: 100.0,
            high: 110.0,
            low: 99.0,
            close: 105.5,
            volume: 0,
            oi: 0,
        };
        w.append_row(&row).expect("append");
        assert_eq!(w.pending(), 1);
        let bytes = w.buffer_bytes();
        let text = String::from_utf8_lossy(bytes);
        assert!(text.contains("prev_day_ohlcv"), "table name in wire bytes");
        assert!(text.contains("feed=dhan"), "feed symbol in wire bytes");
        assert!(text.contains("IDX_I"), "segment in wire bytes");
        assert!(text.contains("oi="), "oi column in wire bytes");
    }

    #[test]
    fn test_append_row_serialises_nonzero_oi_for_fno() {
        // F&O contract: OI is meaningful and must reach the wire.
        let mut w = PrevDayOhlcvWriter::for_test();
        let row = PrevDayOhlcvRow {
            feed: "dhan",
            ts_ist_nanos: 1_000_000_000,
            security_id: 49_081,
            segment: "NSE_FNO",
            open: 250.0,
            high: 260.0,
            low: 245.0,
            close: 255.0,
            volume: 12_345,
            oi: 987_654,
        };
        w.append_row(&row).expect("append");
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(
            text.contains("oi=987654"),
            "F&O oi value on the wire: {text}"
        );
    }

    #[test]
    fn test_flush_when_disconnected_errors() {
        let mut w = PrevDayOhlcvWriter::for_test();
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }

    #[test]
    fn test_two_appends_increment_pending() {
        let mut w = PrevDayOhlcvWriter::for_test();
        let row = PrevDayOhlcvRow {
            feed: "dhan",
            ts_ist_nanos: 1_000_000_000,
            security_id: 25,
            segment: "NSE_EQ",
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 100,
            oi: 0,
        };
        w.append_row(&row).expect("a1");
        w.append_row(&row).expect("a2");
        assert_eq!(w.pending(), 2);
    }
}
