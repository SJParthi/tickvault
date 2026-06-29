//! Groww live-tick persistence — second feed (operator lock 2026-06-19 §32,
//! `groww-second-feed-scope-2026-06-19.md`). **Updated 2026-06-19 ("same tables
//! + feed column"):** Groww writes the **SHARED Dhan `ticks` table**, tagged
//! `feed='groww'` — there is NO separate `groww_live_ticks` table anymore. The
//! feeds are distinguished by the `feed` SYMBOL column + the shared DEDUP key
//! `(ts, security_id, segment, capture_seq, feed)`, so a Dhan tick and a Groww
//! tick for the same instant are BOTH kept (never collide). The Dhan path is
//! byte-identical when Groww is OFF (default). Groww's MILLISECOND precision is
//! preserved because the designated `ts` is the producer's IST nanos verbatim.
//!
//! ## Schema (the SHARED `ticks` table — see `tick_persistence.rs`)
//!
//! Groww writes a subset of the shared `ticks` columns:
//! `feed='groww'`, `segment`, `security_id`, `ltp`, `volume`,
//! `exchange_timestamp` (IST epoch seconds), `capture_seq`, and the designated
//! `ts` (IST nanos, ms-precise). Unset `ticks` columns (open/high/low/close/oi/…)
//! are null for an LTP feed. DEDUP UPSERT KEYS = `(ts, security_id, segment,
//! capture_seq, feed)`.
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
//! producer's job, discovered against the live Groww feed). **Groww's
//! millisecond precision IS preserved** — the designated `ts` column stores the
//! full IST nanoseconds (`ts_ist_nanos`), so the ms is recoverable from `ts`
//! itself (the shared `ticks` table has no separate ms column, and the Dhan
//! feed has none either). The `GrowwLiveTickRow::exchange_ts_millis` field below
//! is the raw producer ms kept on the in-memory row for reference/debugging; it
//! is NOT a stored column (the canonical ms is the nanos `ts`).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender};
use tracing::warn;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed::Feed;

use crate::tick_row_builder::{RawTickFields, build_tick_row_for_feed};

/// The SHARED `ticks` table — Groww writes here too (operator decision
/// 2026-06-19, "same tables + feed column"), distinguished by `feed='groww'`.
/// Its schema + DEDUP key `(ts, security_id, segment, capture_seq, feed)` live in
/// `tick_persistence.rs` and are ensured at boot via `ensure_tick_table_dedup_keys`.
pub const SHARED_TICKS_TABLE: &str = "ticks";

/// Broker-source label for Groww rows in the shared `ticks` table. `&'static str`
/// → zero-alloc, O(1) symbol write. Mirrors `tick_persistence::TICK_FEED_DHAN`.
pub const GROWW_FEED_LABEL: &str = tickvault_common::feed::Feed::Groww.as_str();

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
    /// Raw Groww millisecond timestamp (reference/debug only — NOT written as a
    /// column; the canonical ms-precise value is the designated `ts` nanos).
    pub exchange_ts_millis: i64,
    /// Monotonic, replay-stable dedup tiebreaker (TICK-SEQ-01).
    pub capture_seq: i64,
}

/// Ensure the SHARED `ticks` table + its DEDUP keys exist — Groww writes here
/// tagged `feed='groww'` (operator decision 2026-06-19, "same tables + feed
/// column"). Delegates to the canonical
/// `tick_persistence::ensure_tick_table_dedup_keys` so the schema AND the
/// `(ts, security_id, segment, capture_seq, feed)` key are IDENTICAL to Dhan's —
/// there is no separate Groww ticks table. Invoked from the `groww_enabled` boot
/// block so Groww-only mode (which skips the Dhan boot) still has `ticks` ready.
// TEST-EXEMPT: thin delegate to the tested shared ensurer (needs live QuestDB).
pub async fn ensure_groww_live_ticks_table(questdb_config: &QuestDbConfig) {
    crate::tick_persistence::ensure_tick_table_dedup_keys(questdb_config).await;
}

/// Lazy-connect, self-reconnecting ILP writer for Groww rows in the SHARED
/// `ticks` table. Mirrors the Dhan `TickPersistenceWriter` resilience pattern:
/// if QuestDB is unreachable at construction the writer still builds
/// (`sender = None`) and `append_row` fills the local buffer; the NEXT `flush`
/// then attempts a reconnect via the retained `ilp_conf` string before flushing.
/// On a flush error from an EXISTING sender, `sender` is set back to `None` so
/// the following flush reconnects (drop-and-reconnect). The buffer is RETAINED on
/// any flush/reconnect failure — no buffered row is ever silently discarded. The
/// durable floor for Groww ticks is the producer's capture-at-receipt file
/// (lock §32), so a transient flush failure here is recoverable, not data loss.
///
/// **The bug this fixes (2026-06-29):** the previous fire-once design had NO
/// reconnect — if `Sender::from_conf` failed at `new()` (e.g. the bridge is
/// spawned before the QuestDB-ready gate), `sender` stayed `None` for the whole
/// process. Every `append_row` still succeeded into the in-RAM buffer, but every
/// `flush` returned `Err`, so the buffer never drained and ZERO rows reached the
/// `ticks` table for `feed='groww'` — even though the dashboard counter (then
/// bumped per append) showed hundreds of thousands of "ticks". The honest
/// counter (now bumped per PERSISTED row on a successful flush) lives in the
/// bridge; the reconnect lives here.
pub struct GrowwLiveTickWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// Retained ILP conf string for reconnection (mirrors
    /// `TickPersistenceWriter::ilp_conf_string`). Holds NO secret (host + port
    /// only), so it is safe to keep and never logged with a token.
    ilp_conf: String,
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
                    ilp_conf: conf,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "groww_live_ticks writer: QuestDB unreachable — buffering locally (will reconnect on next flush)"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    ilp_conf: conf,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer. The conf points at an
    /// unreachable loopback port so a `flush()` reconnect attempt fails fast.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            // Port 1 is reserved/refused on every platform → reconnect fails fast
            // in the disconnected-flush test without hanging.
            ilp_conf: "tcp::addr=127.0.0.1:1;".to_string(),
        }
    }

    /// The retained ILP conf string (observability + reconnect test).
    #[must_use]
    pub fn ilp_conf(&self) -> &str {
        &self.ilp_conf
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

    /// Append one Groww tick to the ILP buffer — written to the SHARED `ticks`
    /// table tagged `feed='groww'` (operator decision 2026-06-19, "same tables +
    /// feed column"). The designated `ts` is `row.ts_ist_nanos` directly, so
    /// Groww's MILLISECOND precision is preserved (the sidecar already converted
    /// ms → IST nanos). Uniqueness is the shared key `(ts, security_id, segment,
    /// capture_seq, feed)` — a Dhan tick and this Groww tick for the same instant
    /// are BOTH kept. Symbols (`segment`, `feed`) precede all columns per ILP.
    /// O(1), zero alloc beyond the buffer's internal growth.
    pub fn append_row(&mut self, row: &GrowwLiveTickRow) -> Result<()> {
        // C1 convergence: emit the row via the ONE shared `ticks` builder. Groww
        // is an LTP-only feed, so every Dhan-only column is `None` → the builder
        // OMITS its token → the cell is NULL (never `0`). The designated `ts` is
        // `row.ts_ist_nanos` verbatim, so Groww's MILLISECOND precision is
        // preserved. `ltp` is native f64 (no f32→f64 widening — that is a
        // Dhan-WebSocket-f32 concern, not Groww's; `data-integrity.md`).
        let fields = RawTickFields {
            security_id: row.security_id,
            segment: row.segment,
            ltp: row.ltp,
            volume: row.volume,
            ts_ist_nanos: row.ts_ist_nanos,
            capture_seq: row.capture_seq,
            // `exchange_timestamp` LONG = IST epoch SECONDS (mirrors the Dhan
            // column); the full ms lives in the designated `ts`.
            exchange_timestamp: Some(row.ts_ist_nanos / 1_000_000_000),
            // Groww supplies none of the OHLC / OI / qty / avg_price /
            // payload_hash / received_at columns → NULL (not 0).
            open: None,
            high: None,
            low: None,
            close: None,
            oi: None,
            avg_price: None,
            last_trade_qty: None,
            total_buy_qty: None,
            total_sell_qty: None,
            received_at_ist_nanos: None,
            payload_hash: None,
        };
        build_tick_row_for_feed(&mut self.buffer, &fields, Feed::Groww)
            .with_context(|| "groww ticks append: shared row build failed")?;
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

    /// Flush the buffer to QuestDB, returning the number of rows ACTUALLY
    /// persisted (so the caller can count persisted rows, not buffer appends).
    ///
    /// Resilience (mirrors the Dhan `TickPersistenceWriter`):
    /// - If `sender` is `None` (never connected, or dropped after a prior flush
    ///   error), reconnect via the retained `ilp_conf` BEFORE flushing. The
    ///   reconnect creates a FRESH `Buffer` bound to the new sender, so any rows
    ///   appended while disconnected are migrated into it first (no row lost).
    /// - On a flush error from an EXISTING sender, set `sender = None` so the NEXT
    ///   flush reconnects (drop-and-reconnect). The buffer is RETAINED.
    /// - `Err` if (re)connect or flush fails — the buffer + `pending` are kept and
    ///   the producer's capture-at-receipt file remains the durable record (no
    ///   data loss). The caller logs the error.
    pub fn flush(&mut self) -> Result<usize> {
        if self.sender.is_none() {
            self.reconnect()
                .context("groww_live_ticks flush: reconnect to QuestDB failed")?;
        }
        let sender = self
            .sender
            .as_mut()
            .context("groww_live_ticks flush: not connected to QuestDB")?;
        if let Err(err) = sender.flush(&mut self.buffer) {
            // Drop the broken sender so the next flush reconnects. The buffer is
            // RETAINED (questdb's `flush` does not clear it on error), so no row
            // is lost — it is re-attempted on the next flush.
            self.sender = None;
            return Err(err).context("groww_live_ticks flush: ILP flush failed");
        }
        let flushed = self.pending;
        self.pending = 0;
        Ok(flushed)
    }

    /// (Re)establish the ILP sender from the retained conf. The EXISTING buffer is
    /// KEPT — its already-appended rows are flushed by the new sender (both the
    /// disconnected `Buffer::new(ProtocolVersion::V1)` and a connected
    /// `new_buffer()` use the V1 TCP line protocol with the default name-length
    /// limit, so the buffer is valid to flush through the fresh sender). This
    /// means rows appended while disconnected are NOT lost — they drain on the
    /// first successful flush after reconnect. Cold path: only runs when `sender`
    /// is `None`.
    fn reconnect(&mut self) -> Result<()> {
        let new_sender =
            Sender::from_conf(&self.ilp_conf).context("groww_live_ticks: ILP reconnect failed")?;
        self.sender = Some(new_sender);
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
    fn test_groww_writes_shared_ticks_table_tagged_feed_groww() {
        // Operator decision 2026-06-19 ("same tables + feed column"): Groww
        // writes the SHARED `ticks` table, NOT a separate groww_* table.
        assert_eq!(SHARED_TICKS_TABLE, "ticks");
        assert_eq!(GROWW_FEED_LABEL, "groww");
    }

    #[test]
    fn test_for_test_writer_is_disconnected_and_empty() {
        let w = GrowwLiveTickWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_append_row_writes_ticks_with_feed_groww_and_ms_ts() {
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let text = String::from_utf8_lossy(w.buffer_bytes());
        // SHARED `ticks` table, tagged feed=groww (NOT a groww_* table).
        assert!(
            text.starts_with("ticks,"),
            "shared ticks table on wire: {text}"
        );
        assert!(
            !text.contains("groww_live_ticks"),
            "must NOT use groww_* table"
        );
        assert!(
            text.contains("feed=groww"),
            "feed=groww tag on wire: {text}"
        );
        assert!(text.contains("NSE_EQ"), "segment on wire");
        // ms precision is preserved in the designated `ts` (ts_ist_nanos verbatim).
        assert!(
            text.contains("1780000000123000000"),
            "ms-precise ts (nanos) on wire: {text}"
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
        // for_test()'s conf points at refused port 1 → reconnect fails → flush Errs
        // (never panics). Returns Result<usize> now; .is_err() still holds.
        assert!(w.flush().is_err(), "disconnected flush must Err, not panic");
    }

    #[test]
    fn test_writer_stores_conf_for_reconnect() {
        // The retained conf is what flush() uses to reconnect; it must be present
        // and must NOT contain a token/secret (host+port only).
        let w = GrowwLiveTickWriter::for_test();
        assert!(
            w.ilp_conf().starts_with("tcp::addr="),
            "conf: {}",
            w.ilp_conf()
        );
        assert!(
            !w.ilp_conf().to_lowercase().contains("token"),
            "ilp conf must not carry a token/secret"
        );
    }

    #[test]
    fn test_disconnected_flush_retains_buffer_and_pending() {
        // The bug fix's core invariant: a failed flush (reconnect to refused port)
        // must NOT discard the buffered rows — they stay for the next flush.
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        let bytes_before = w.buffer_byte_count();
        assert_eq!(w.pending(), 1);
        assert!(w.flush().is_err(), "reconnect to refused port must Err");
        assert_eq!(
            w.pending(),
            1,
            "pending must be retained on flush failure (no silent loss)"
        );
        assert_eq!(
            w.buffer_byte_count(),
            bytes_before,
            "buffer bytes must be retained on flush failure"
        );
    }

    #[test]
    fn test_two_appends_increment_pending() {
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("a1");
        w.append_row(&sample_row()).expect("a2");
        assert_eq!(w.pending(), 2);
    }
}
