//! Groww 1-minute candle persistence — second feed (operator lock §32 +
//! "same tables + feed column" 2026-06-19, `groww-second-feed-scope-2026-06-19.md`).
//!
//! **Groww writes the SHARED Dhan `candles_1m` table, tagged `feed='groww'`** —
//! NOT a separate `groww_candles_1m` table (that namespace was RETIRED in the
//! same-tables pivot, mirroring the ticks side in Step 3a). The two feeds
//! coexist in one table, distinguished by the `feed` column which is part of the
//! shared candle DEDUP key `(ts, security_id, segment, feed)` (`DEDUP_KEY_CANDLES`
//! in `shadow_persistence.rs`). So a Dhan candle and a Groww candle for the SAME
//! `(ts, security_id, segment)` minute are BOTH kept — distinct feeds = distinct
//! observations, never a collision — and "Groww candles only" is an O(1)
//! `WHERE feed = 'groww'` filter, never a second table.
//!
//! The live-1m-vs-backtest-1m parity check (the goal of the second feed) reads
//! `candles_1m WHERE feed = 'groww'` and compares Groww-against-Groww; the Dhan
//! rows (`feed = 'dhan'`) are never touched by that compare.
//!
//! ## Schema (owned by `shadow_persistence::ensure_shadow_candle_tables`)
//!
//! The shared `candles_1m` table has the full 15-column Dhan schema. Groww writes
//! a strict SUBSET — `feed, segment, security_id, ts, open, high, low, close,
//! volume, tick_count` — leaving `oi` + the 4 `*_pct*` columns NULL for Groww
//! rows (Groww does not compute OI or prev-day %). QuestDB ILP permits a
//! per-row column subset; the unwritten columns are NULL for that row only.
//!
//! ## Uniqueness / dedup (I-P1-11 + feed)
//!
//! The shared candle DEDUP key `(ts, security_id, segment, feed)` is the single
//! source of truth (`DEDUP_KEY_CANDLES` / `TfIndex::dedup_key()`). A sealed
//! 1-minute candle is uniquely identified by its minute boundary + instrument +
//! feed, so re-sealing the same Groww minute UPSERTs in place (idempotent).
//!
//! ## Timestamp rule (`data-integrity.md`)
//!
//! `ts_ist_nanos` is the minute-aligned IST designated timestamp supplied by the
//! aggregator (already IST — NO offset applied here).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::warn;

use tickvault_common::config::QuestDbConfig;
use tickvault_trading::candles::TfIndex;

/// One Groww 1-minute candle ready for ILP write into the SHARED `candles_1m`
/// table. All prices `f64` (the Groww SDK emits native floats — no
/// `f32_to_f64_clean` widening concern).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowwCandle1mRow {
    /// Designated timestamp: minute-aligned IST nanoseconds. NO offset here.
    pub ts_ist_nanos: i64,
    /// Composite-key part 1 (I-P1-11) — Groww exchange_token, widened to `i64`.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11). `&'static` segment string for the
    /// `symbol` column.
    pub segment: &'static str,
    /// Broker-source provenance label (e.g. `"groww"`). Part of the shared
    /// candle DEDUP key — keeps Groww candles distinct from Dhan candles.
    pub feed: &'static str,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    /// Number of ticks folded into this candle (observability; NON-key).
    pub tick_count: i64,
}

/// Ensure the Groww candle target table exists. Groww writes the SHARED
/// `candles_1m` table, so this DELEGATES to the canonical candle-table DDL —
/// exactly mirroring how `groww_persistence::ensure_groww_live_ticks_table`
/// delegates to the shared ticks DDL (Step 3a). Only invoked when the Groww feed
/// is enabled, so that Groww-only mode (which skips the Dhan boot, where the
/// candle tables are otherwise ensured) still has `candles_1m` + its 4-col feed
/// DEDUP key before the bridge writes.
///
/// **Drops legacy candle matviews FIRST** (3-agent hostile-review MEDIUM,
/// 2026-06-19): a pre-`#T1` Engine-C materialized view named `candles_1m` makes
/// `CREATE TABLE IF NOT EXISTS candles_1m` a silent no-op, after which ILP
/// appends to that name FAIL (matviews are not ILP-writable). The Dhan boot
/// always calls `drop_legacy_candle_objects` before `ensure_shadow_candle_tables`
/// for exactly this reason; a Groww-ONLY run on a brownfield QuestDB volume must
/// do the same or Groww candles would silently never persist. Both calls are
/// idempotent (marker-gated drop + `IF NOT EXISTS` create), so this is safe to
/// run in every mode even when the Dhan path also runs them.
// TEST-EXEMPT: thin delegation to unit-tested drop_legacy_candle_objects + ensure_shadow_candle_tables.
pub async fn ensure_groww_candles_1m_table(questdb_config: &QuestDbConfig) {
    crate::shadow_persistence::drop_legacy_candle_objects(questdb_config).await;
    crate::shadow_persistence::ensure_shadow_candle_tables(questdb_config).await;
}

/// Lazy-connect ILP writer for Groww 1-minute candles into the SHARED
/// `candles_1m` table. Mirrors `PrevDayOhlcvWriter`: if QuestDB is unreachable at
/// construction the writer still builds (`sender = None`); `append_row` fills the
/// local buffer and `flush` returns `Err` until a reconnect lands.
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
                    "groww candles writer: QuestDB unreachable — buffering locally"
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

    /// Append one Groww 1m candle row to the SHARED `candles_1m` ILP buffer.
    /// O(1), zero alloc beyond the buffer's internal growth. The target table is
    /// `TfIndex::M1.table_name()` (the single source of truth for the candle
    /// table name) — never a separate `groww_*` table. `feed` is stamped as a
    /// SYMBOL (part of the DEDUP key) so Groww rows never collide with Dhan rows.
    pub fn append_row(&mut self, row: &GrowwCandle1mRow) -> Result<()> {
        self.buffer
            .table(TfIndex::M1.table_name())
            .with_context(|| "groww candle append: table() failed")?
            .symbol("feed", row.feed)
            .with_context(|| "groww candle append: symbol(feed) failed")?
            .symbol("segment", row.segment)
            .with_context(|| "groww candle append: symbol(segment) failed")?
            .column_i64("security_id", row.security_id)
            .with_context(|| "groww candle append: column_i64(security_id) failed")?
            .column_f64("open", row.open)
            .with_context(|| "groww candle append: column_f64(open) failed")?
            .column_f64("high", row.high)
            .with_context(|| "groww candle append: column_f64(high) failed")?
            .column_f64("low", row.low)
            .with_context(|| "groww candle append: column_f64(low) failed")?
            .column_f64("close", row.close)
            .with_context(|| "groww candle append: column_f64(close) failed")?
            .column_i64("volume", row.volume)
            .with_context(|| "groww candle append: column_i64(volume) failed")?
            .column_i64("tick_count", row.tick_count)
            .with_context(|| "groww candle append: column_i64(tick_count) failed")?
            .at(TimestampNanos::new(row.ts_ist_nanos))
            .with_context(|| "groww candle append: at(ts) failed")?;
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
            .context("groww candles flush: not connected to QuestDB")?;
        sender
            .flush(&mut self.buffer)
            .context("groww candles flush: ILP flush failed")?;
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
    fn test_for_test_writer_is_disconnected_and_empty() {
        let w = GrowwCandle1mWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_append_row_writes_shared_candles_1m_tagged_feed_groww() {
        // Same-tables pivot (operator 2026-06-19): Groww candles go to the SHARED
        // `candles_1m` table tagged `feed=groww` — NOT a separate `groww_*` table.
        let mut w = GrowwCandle1mWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        assert!(
            text.contains("candles_1m"),
            "shared candle table name on wire"
        );
        assert!(
            !text.contains("groww_candles_1m"),
            "must NOT write a separate groww_candles_1m table (namespace retired)"
        );
        assert!(text.contains("feed=groww"), "feed provenance on wire");
        assert!(text.contains("NSE_EQ"), "segment on wire");
        assert!(text.contains("tick_count=87"), "tick_count on wire");
    }

    #[test]
    fn test_target_table_is_the_shared_m1_candle_table() {
        // The single source of truth for the candle table name is
        // `TfIndex::M1.table_name()` — Groww writes exactly that, so the two
        // feeds share one table (distinguished only by the `feed` column).
        assert_eq!(TfIndex::M1.table_name(), "candles_1m");
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
