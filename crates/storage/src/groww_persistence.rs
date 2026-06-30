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
use questdb::ErrorCode as QuestErrorCode;
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

/// Max in-wake reconnect+replay attempts on a flush that failed with a CONNECTION
/// error (broken pipe / connection reset / not-connected). Bounded so a sustained
/// QuestDB outage degrades to the producer's capture-at-receipt spill/DLQ net
/// (lock §32) instead of stalling the bridge wake forever. After these are
/// exhausted, `flush` returns `Err` with the buffer + pending RETAINED.
pub const GROWW_FLUSH_RECONNECT_MAX_RETRIES: usize = 3;

/// Exponential backoff (milliseconds) slept BETWEEN reconnect attempts. Indexed
/// by `attempt - 1`. Total wall-clock across all 3 retries ≤ 350ms, so the bridge
/// wake is never blocked for long — well inside the open-burst recovery budget.
pub const GROWW_FLUSH_RECONNECT_BACKOFF_MS: [u64; GROWW_FLUSH_RECONNECT_MAX_RETRIES] =
    [50, 100, 200];

/// Outcome of a successful `GrowwLiveTickWriter::flush`. Carries the number of
/// rows ACTUALLY persisted (so the caller counts persisted rows, not buffer
/// appends) AND whether the flush had to reconnect+replay after a transient
/// connection error — so the caller can log a quiet `info!` recovery instead of a
/// scary `error!` (the `error!` is reserved for the true give-up-to-spill path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrowwFlushOutcome {
    /// Rows confirmed written to QuestDB on the flush that finally succeeded.
    pub persisted: usize,
    /// `true` if at least one reconnect+replay was needed before success.
    pub reconnected: bool,
}

/// Pure classifier: is this questdb error a recoverable CONNECTION fault (so a
/// reconnect + replay is worth attempting), as opposed to a structural error
/// (bad name / API misuse / server-rejected row) that re-sending would NOT fix?
///
/// Only `ErrorCode::SocketError` is a transport fault — questdb-rs maps every
/// broken-pipe / connection-reset / "not connected to database" `io::Error` to
/// `SocketError` (questdb-rs `sender/mod.rs` `map_io_to_socket_err` +
/// `flush_impl`'s not-connected guard). Every other code (`InvalidName`,
/// `InvalidApiCall`, `ServerFlushError`, `InvalidTimestamp`, …) is a property of
/// the buffered row or the request, NOT the socket — retrying re-hammers a row
/// that will fail again, so those return `false`. O(1), pure, no I/O.
#[must_use]
pub fn is_connection_error(err: &questdb::Error) -> bool {
    matches!(err.code(), QuestErrorCode::SocketError)
}

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

    /// Flush the buffer to QuestDB with IN-WAKE reconnect + idempotent replay on a
    /// transient connection error, returning a [`GrowwFlushOutcome`] (rows actually
    /// persisted + whether a reconnect was needed).
    ///
    /// Zero-drop-at-open contract (operator directive 2026-06-30): the previous
    /// design dropped the broken sender and deferred recovery to the NEXT flush
    /// wake — so an open-bell socket reset (`Broken pipe (os error 32)`) left the
    /// retained rows unflushed until the next tick, and the spill net absorbed the
    /// gap. This now reconnects + re-flushes the SAME buffer IN this wake, bounded
    /// to [`GROWW_FLUSH_RECONNECT_MAX_RETRIES`] with [`GROWW_FLUSH_RECONNECT_BACKOFF_MS`]
    /// backoff, BEFORE falling through to the durable spill/DLQ net.
    ///
    /// Resilience contract:
    /// - **Connection error** (`is_connection_error` ⇒ `ErrorCode::SocketError`,
    ///   i.e. broken pipe / reset / not-connected): drop the broken sender,
    ///   reconnect via the retained `ilp_conf`, and re-flush the SAME buffer. The
    ///   buffer is RETAINED on a questdb flush error (`Sender::flush` only
    ///   `buf.clear()`s on `Ok`), so the retained rows are re-sent verbatim. Replay
    ///   is idempotent: the shared `ticks` DEDUP key
    ///   `(ts, security_id, segment, capture_seq, feed)` collapses any row a
    ///   partial write had already landed before the pipe broke — never a double
    ///   count.
    /// - **Non-connection error** (`InvalidName`, `InvalidApiCall`, `ServerFlushError`,
    ///   …): NOT retried — re-sending a structurally bad row fails again. Returns
    ///   `Err` immediately (sender dropped so a later wake can reconnect).
    /// - **Retries exhausted** (sustained QuestDB outage): returns `Err` with the
    ///   buffer + `pending` RETAINED. The producer's capture-at-receipt spill/DLQ
    ///   file (lock §32) remains the durable floor — the caller logs `error!` and
    ///   the rows are re-attempted on the next wake. Never an infinite loop.
    pub fn flush(&mut self) -> Result<GrowwFlushOutcome> {
        let mut reconnected = false;
        // Attempt 0 is the first flush; attempts 1..=N are reconnect+replay.
        for attempt in 0..=GROWW_FLUSH_RECONNECT_MAX_RETRIES {
            // (Re)connect if we have no live sender (cold start, or a prior
            // connection-error arm dropped it). The retained buffer is KEPT.
            if self.sender.is_none()
                && let Err(err) = self.reconnect()
            {
                // Reconnect itself failed — treat as a failed attempt. If we
                // still have retries left, back off and try again; otherwise
                // surface the error with the buffer + pending RETAINED.
                if attempt == GROWW_FLUSH_RECONNECT_MAX_RETRIES {
                    return Err(err).context("groww_live_ticks flush: reconnect to QuestDB failed");
                }
                metrics::counter!("tv_groww_ilp_reconnect_attempts_total").increment(1);
                reconnected = true;
                Self::backoff_sleep(attempt);
                continue;
            }

            match self.try_flush_once() {
                Ok(flushed) => {
                    if reconnected {
                        metrics::counter!("tv_groww_ilp_reconnect_recoveries_total").increment(1);
                    }
                    return Ok(GrowwFlushOutcome {
                        persisted: flushed,
                        reconnected,
                    });
                }
                Err(err) => {
                    // Drop the broken sender so the next iteration reconnects. The
                    // buffer is RETAINED (questdb's `flush` does not clear it on
                    // error), so no row is lost — it is re-attempted below or on a
                    // later wake.
                    self.sender = None;

                    // A structural (non-connection) error will NOT be fixed by a
                    // reconnect — re-sending the same bad row fails again. Surface
                    // it immediately so a genuinely bad row is not re-hammered.
                    if !is_connection_error(&err) {
                        return Err(err)
                            .context("groww_live_ticks flush: ILP flush failed (non-connection)");
                    }

                    // Connection error: out of retries → fall through to the
                    // durable spill/DLQ net (the caller logs error! + records no
                    // ticks). Buffer + pending RETAINED.
                    if attempt == GROWW_FLUSH_RECONNECT_MAX_RETRIES {
                        return Err(err).context(
                            "groww_live_ticks flush: ILP flush failed after reconnect retries",
                        );
                    }

                    // Retry: count the attempt, back off, reconnect+replay.
                    metrics::counter!("tv_groww_ilp_reconnect_attempts_total").increment(1);
                    reconnected = true;
                    Self::backoff_sleep(attempt);
                }
            }
        }
        // Unreachable: the loop returns on success, on a non-connection error, or
        // on the final attempt. Defensive anyhow error keeps the buffer RETAINED.
        Err(anyhow::anyhow!(
            "groww_live_ticks flush: exhausted reconnect retries"
        ))
    }

    /// Single flush attempt against the CURRENT sender, returning the raw questdb
    /// result so `flush` can classify the error. On `Ok`, `pending` is reset (the
    /// rows are confirmed written); on `Err`, `pending` + buffer are LEFT intact.
    fn try_flush_once(&mut self) -> std::result::Result<usize, questdb::Error> {
        let sender = match self.sender.as_mut() {
            Some(s) => s,
            None => {
                return Err(questdb::Error::new(
                    QuestErrorCode::SocketError,
                    "groww_live_ticks: not connected to QuestDB",
                ));
            }
        };
        sender.flush(&mut self.buffer)?;
        let flushed = self.pending;
        self.pending = 0;
        Ok(flushed)
    }

    /// Sleep the backoff for a given 0-based attempt index. Bounded by
    /// [`GROWW_FLUSH_RECONNECT_BACKOFF_MS`]; a synchronous sleep is acceptable here
    /// because this flush runs on the bridge's blocking persist path and the total
    /// across all retries is ≤ 350ms (well inside the open-burst recovery budget).
    fn backoff_sleep(attempt: usize) {
        if let Some(&ms) = GROWW_FLUSH_RECONNECT_BACKOFF_MS.get(attempt) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
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
        // for_test()'s conf points at refused port 1 → every reconnect fails → the
        // bounded reconnect+replay loop exhausts and flush Errs (never panics).
        // Returns Result<GrowwFlushOutcome> now; .is_err() still holds.
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

    // ---- reconnect + replay (zero-drop-at-open, operator directive 2026-06-30) ----

    #[test]
    fn test_is_connection_error_true_for_socket_error() {
        // A broken pipe / connection reset / not-connected all surface from
        // questdb-rs as ErrorCode::SocketError — the ONLY class worth a reconnect.
        let err = questdb::Error::new(
            QuestErrorCode::SocketError,
            "Could not flush buffer: Broken pipe (os error 32)",
        );
        assert!(
            is_connection_error(&err),
            "SocketError must classify as a connection error (retry-worthy)"
        );
    }

    #[test]
    fn test_is_connection_error_false_for_non_socket_error() {
        // Structural errors must NOT be retried — re-sending a bad row fails again.
        for code in [
            QuestErrorCode::InvalidName,
            QuestErrorCode::InvalidApiCall,
            QuestErrorCode::ServerFlushError,
            QuestErrorCode::InvalidTimestamp,
            QuestErrorCode::CouldNotResolveAddr,
        ] {
            let err = questdb::Error::new(code, "structural failure");
            assert!(
                !is_connection_error(&err),
                "non-SocketError code {code:?} must NOT classify as a connection error"
            );
        }
    }

    #[test]
    fn test_reconnect_backoff_schedule_caps_at_three() {
        // The retry budget is bounded — total wall-clock ≤ 350ms so the bridge
        // wake never stalls, and a sustained outage degrades to the spill net.
        assert_eq!(GROWW_FLUSH_RECONNECT_MAX_RETRIES, 3);
        assert_eq!(GROWW_FLUSH_RECONNECT_BACKOFF_MS, [50, 100, 200]);
        assert_eq!(
            GROWW_FLUSH_RECONNECT_BACKOFF_MS.len(),
            GROWW_FLUSH_RECONNECT_MAX_RETRIES,
            "backoff schedule length must match the retry count"
        );
        let total_ms: u64 = GROWW_FLUSH_RECONNECT_BACKOFF_MS.iter().sum();
        assert!(
            total_ms <= 350,
            "total backoff must stay bounded (≤350ms), got {total_ms}ms"
        );
        // The schedule is monotonic non-decreasing (exponential-ish).
        assert!(
            GROWW_FLUSH_RECONNECT_BACKOFF_MS
                .windows(2)
                .all(|w| w[0] <= w[1]),
            "backoff must be non-decreasing"
        );
    }

    #[test]
    fn test_backoff_sleep_out_of_range_is_noop() {
        // An attempt index past the schedule (defensive) must not panic — the
        // get() returns None and the sleep is skipped.
        GrowwLiveTickWriter::backoff_sleep(GROWW_FLUSH_RECONNECT_MAX_RETRIES + 5);
    }

    #[test]
    fn test_flush_outcome_struct_fields() {
        // The outcome distinguishes a clean flush from a recovered one so the
        // caller can log info! (recovered) vs error! (gave up to spill).
        let clean = GrowwFlushOutcome {
            persisted: 7,
            reconnected: false,
        };
        let recovered = GrowwFlushOutcome {
            persisted: 7,
            reconnected: true,
        };
        assert_eq!(clean.persisted, 7);
        assert!(!clean.reconnected);
        assert!(recovered.reconnected);
        assert_ne!(clean, recovered, "reconnected flag must be observable");
    }

    #[test]
    fn test_disconnected_flush_retains_buffer_and_pending_after_retries() {
        // Zero-loss invariant after the new bounded retry loop: a flush that
        // exhausts all reconnect attempts (refused port) must STILL retain the
        // buffered rows — they fall through to the producer's spill net and are
        // re-attempted on the next wake. No silent discard.
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        let bytes_before = w.buffer_byte_count();
        assert_eq!(w.pending(), 1);
        assert!(
            w.flush().is_err(),
            "exhausted reconnect retries must Err (refused port)"
        );
        assert_eq!(
            w.pending(),
            1,
            "pending must be retained after exhausted retries (no silent loss)"
        );
        assert_eq!(
            w.buffer_byte_count(),
            bytes_before,
            "buffer bytes must be retained after exhausted retries"
        );
    }

    #[test]
    fn test_try_flush_once_when_sender_none_returns_socket_error() {
        // try_flush_once with no sender returns a SocketError (the retry-worthy
        // class) rather than panicking — so the loop reconnects, not gives up.
        let mut w = GrowwLiveTickWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        let err = w
            .try_flush_once()
            .expect_err("no sender must Err, not flush");
        assert!(
            is_connection_error(&err),
            "no-sender error must be a connection error so the loop reconnects"
        );
        // Buffer + pending untouched on the failed attempt.
        assert_eq!(w.pending(), 1);
    }
}
