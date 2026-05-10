//! Wave 6 Sub-PR #1 item 1.9 — `aggregator_seal_audit` ILP writer.
//!
//! Per-seal forensic audit table (DDL already in
//! `shadow_persistence.rs::ensure_shadow_candle_tables`). One row
//! emitted per sealed candle by the future aggregator wiring (item
//! 1.4); SEBI 5-year retention applies. Composite DEDUP per
//! locked decision **L-M16**:
//! `DEDUP UPSERT KEYS(ts, trading_date_ist, security_id, exchange_segment, timeframe, candle_ts)`.
//!
//! ## Why ILP (not HTTP /exec INSERT)
//!
//! Per-seal audit is high-volume (~5.5M rows/day = 11K instruments
//! × 9 TFs × 56 minute-bars × … even discounting most TFs). Mirrors
//! the ILP throughput pattern of `ShadowCandleWriter` (item 1.2f.2 /
//! PR #564) so a single tokio-task drain handles both audit and
//! shadow rows without separate HTTP connection pools.
//!
//! ## What this slice ships
//!
//! - [`AggregatorSealAuditOutcome`] — typed enum mapping to the
//!   `outcome` SYMBOL column (`"ok"` / `"dropped"` / `"late_tick"`).
//!   Removes free-text-injection risk and keeps the SYMBOL set
//!   bounded for QuestDB.
//! - [`AggregatorSealAuditWriter`] — lazy-connect ILP writer with
//!   the same API surface as `ShadowCandleWriter`:
//!   - `new(config)` — production constructor (never errs on
//!     QuestDB-unreachable; drops to disconnected mode).
//!   - `for_test()` — test constructor, disconnected.
//!   - `append(...)` — fills the ILP buffer for one audit row.
//!   - `flush()` — sends; returns `Err` when disconnected with
//!     `pending_count` preserved (rescue invariant — the future
//!     writer task can re-route via the same spill cascade as
//!     shadow rows).
//!   - Observability: `is_connected`, `pending_count`,
//!     `buffer_byte_count`, `buffer_row_count`, `buffer_bytes`.
//! - 16 unit tests covering: outcome enum mapping; lazy-connect
//!   disconnect-OK construction; append fills the buffer; row count
//!   grows; bytes contain expected table name + column names + IST
//!   timestamps; multi-segment + multi-TF dispatch; I-P1-11
//!   composite-key isolation in the wire bytes; flush-disconnected
//!   returns Err with `pending_count` preserved.
//!
//! ## What this slice does NOT ship
//!
//! - Boot wiring + `tokio::spawn` of an audit-table writer task
//!   (item 1.4).
//! - Reconnect throttle (item 1.4 — same pattern as
//!   ShadowCandleWriter).
//! - Prometheus counter increments per audit-row outcome (item 1.4).
//!
//! ## Hot-path safety
//!
//! `append` is COLD path — the future caller is the writer task
//! (NOT the aggregator's `consume_tick`). Allowed to allocate
//! (the ILP `Buffer` grows internally). The audit row payload is
//! emitted from the writer task slice in item 1.4, not from the
//! hot path.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{debug, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::segment::segment_code_to_str;
use tickvault_trading::candles::TfIndex;

use crate::shadow_persistence::QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT;

/// Build the questdb-rs ILP TCP connection conf string. Mirrors
/// `ShadowCandleWriter` (PR #564) so both writers connect to the
/// same QuestDB ILP endpoint with identical settings.
fn build_ilp_conf_string(host: &str, ilp_port: u16) -> String {
    format!("tcp::addr={host}:{ilp_port};")
}

/// Outcome SYMBOL value for one audit row. Bounded enum keeps
/// QuestDB's SYMBOL cardinality under control (avoids the unbounded
/// SYMBOL leak that free-text would cause).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregatorSealAuditOutcome {
    /// Seal completed and was successfully buffered (or flushed) to
    /// the shadow-table tier. Most common outcome.
    Ok,
    /// Seal arrived at the writer task but the spill+DLQ cascade
    /// reported `Dropped` (caller fired `AGGREGATOR-DROP-01`). The
    /// audit row records the loss for SEBI forensic trail.
    Dropped,
    /// A late tick was discarded by `AggregatorCell::consume_tick`
    /// (per L-C3 — `AGGREGATOR-LATE-01`). Audit row preserves the
    /// fact for forensic analysis.
    LateTickDiscarded,
}

impl AggregatorSealAuditOutcome {
    /// Stable wire-format SYMBOL string for QuestDB. The SYMBOL set
    /// is pinned by the `test_outcome_as_str_*` ratchets so a
    /// future refactor cannot silently change a SYMBOL value (which
    /// would split forensic queries across two values).
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Dropped => "dropped",
            Self::LateTickDiscarded => "late_tick",
        }
    }
}

/// ILP writer for the `aggregator_seal_audit` forensic table.
///
/// Single producer (the future writer task in item 1.4 is the sole
/// owner) — `&mut self` methods are sufficient.
pub struct AggregatorSealAuditWriter {
    /// Production sender. `None` when QuestDB is unreachable; the
    /// future reconnect path (item 1.4) flips it back.
    sender: Option<Sender>,
    /// In-memory ILP wire-format buffer. Reused across batches.
    buffer: Buffer,
    /// Number of audit rows appended since the last successful flush.
    pending_count: usize,
    // APPROVED: read by item 1.4 reconnect path; no callers in this slice.
    #[allow(dead_code)]
    ilp_conf_string: String,
}

impl AggregatorSealAuditWriter {
    /// Production constructor. Connects to QuestDB via ILP TCP. If
    /// QuestDB is unreachable at startup, the writer constructs in
    /// disconnected mode (matches `ShadowCandleWriter::new`).
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
                    "aggregator-seal-audit writer: QuestDB unreachable at startup — will buffer locally until reconnect"
                );
                (None, Buffer::new(ProtocolVersion::V1))
            }
        };
        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Test constructor — disconnected writer with empty buffer.
    /// Used by every unit test in this module.
    #[must_use]
    // TEST-EXEMPT: test-only construction helper used by every test in this module (lazy-connect, append, row-count, byte-content, multi-TF dispatch, I-P1-11 isolation, flush-disconnected).
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending_count: 0,
            ilp_conf_string: String::new(),
        }
    }

    /// `true` when a live ILP `Sender` is held.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Number of audit rows pending flush.
    #[must_use]
    pub const fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// Bytes currently in the ILP wire buffer.
    #[must_use]
    pub fn buffer_byte_count(&self) -> usize {
        self.buffer.len()
    }

    /// Rows currently in the ILP wire buffer.
    #[must_use]
    pub fn buffer_row_count(&self) -> usize {
        self.buffer.row_count()
    }

    /// Raw ILP bytes — used in unit tests to assert column contents.
    #[must_use]
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Append one forensic audit row.
    ///
    /// `ts_ist_nanos` — designated timestamp (when the seal/audit
    /// happened); IST nanos per `data-integrity.md` (NEVER add
    /// `+19_800` offset — the WS LTT carries IST already).
    ///
    /// `trading_date_ist_micros` — the IST trading-date as
    /// microseconds since epoch (QuestDB DATE storage unit).
    /// Caller derives this from the IST date returned by
    /// `tickvault_trading::candles::boundary_calc::trading_date_ist`.
    ///
    /// `candle_ts_ist_nanos` — the sealed candle's bucket-open
    /// timestamp in IST nanos (matches `ShadowSealRow::timestamp_ist_nanos`).
    ///
    /// `seals_emitted` / `seals_dropped` / `late_ticks_discarded` —
    /// counts since the previous audit row for this `(security_id,
    /// segment, tf, candle_ts)` tuple. Per-seal callers pass `1/0/0`,
    /// `0/1/0`, or `0/0/1` depending on the outcome; per-burst
    /// callers may pass aggregated counts.
    #[allow(clippy::too_many_arguments)] // APPROVED: forensic table needs every column from the L-M16 DDL
    pub fn append(
        &mut self,
        ts_ist_nanos: i64,
        trading_date_ist_micros: i64,
        security_id: u32,
        exchange_segment_code: u8,
        tf: TfIndex,
        candle_ts_ist_nanos: i64,
        seals_emitted: i64,
        seals_dropped: i64,
        late_ticks_discarded: i64,
        outcome: AggregatorSealAuditOutcome,
    ) -> Result<()> {
        let segment_str = segment_code_to_str(exchange_segment_code);
        self.buffer
            .table(QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT)
            .with_context(|| "audit append: invalid table name")?
            .symbol("exchange_segment", segment_str)
            .with_context(|| "audit append: symbol(exchange_segment) failed")?
            .symbol("timeframe", tf.display_name())
            .with_context(|| "audit append: symbol(timeframe) failed")?
            .symbol("outcome", outcome.as_str())
            .with_context(|| "audit append: symbol(outcome) failed")?
            .column_i64("security_id", i64::from(security_id))
            .with_context(|| "audit append: column_i64(security_id) failed")?
            .column_i64("trading_date_ist_micros", trading_date_ist_micros)
            .with_context(|| "audit append: column_i64(trading_date_ist_micros) failed")?
            .column_i64("candle_ts_ist_nanos", candle_ts_ist_nanos)
            .with_context(|| "audit append: column_i64(candle_ts_ist_nanos) failed")?
            .column_i64("seals_emitted", seals_emitted)
            .with_context(|| "audit append: column_i64(seals_emitted) failed")?
            .column_i64("seals_dropped", seals_dropped)
            .with_context(|| "audit append: column_i64(seals_dropped) failed")?
            .column_i64("late_ticks_discarded", late_ticks_discarded)
            .with_context(|| "audit append: column_i64(late_ticks_discarded) failed")?
            .at(TimestampNanos::new(ts_ist_nanos))
            .with_context(|| "audit append: at(TimestampNanos) failed")?;
        self.pending_count += 1;
        Ok(())
    }

    /// Flush buffered ILP rows to QuestDB.
    ///
    /// Returns `Err` when disconnected; `pending_count` is preserved
    /// on `Err` so the future writer task can re-route the unflushed
    /// rows to the spill tier (mirrors `ShadowCandleWriter::flush`
    /// rescue invariant).
    pub fn flush(&mut self) -> Result<()> {
        let Some(sender) = self.sender.as_mut() else {
            return Err(anyhow::anyhow!(
                "audit flush: disconnected — sender is None"
            ));
        };
        if self.buffer.is_empty() {
            return Ok(());
        }
        sender
            .flush(&mut self.buffer)
            .context("audit flush: ILP send failed")?;
        debug!(
            flushed_rows = self.pending_count,
            "aggregator-seal-audit writer flushed"
        );
        self.pending_count = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_NSE_EQ};

    fn append_minimal(writer: &mut AggregatorSealAuditWriter, sid: u32, seg: u8, tf: TfIndex) {
        writer
            .append(
                1_716_000_900_000_000_000_i64, // ts IST nanos
                1_716_000_000_000_000_i64,     // trading_date_ist micros
                sid,
                seg,
                tf,
                1_716_000_900_000_000_000_i64, // candle_ts IST nanos
                1,                             // seals_emitted
                0,                             // seals_dropped
                0,                             // late_ticks_discarded
                AggregatorSealAuditOutcome::Ok,
            )
            .expect("append");
    }

    // ── Outcome enum ────────────────────────────────────────────

    #[test]
    fn test_outcome_as_str_ok() {
        assert_eq!(AggregatorSealAuditOutcome::Ok.as_str(), "ok");
    }

    #[test]
    fn test_outcome_as_str_dropped() {
        assert_eq!(AggregatorSealAuditOutcome::Dropped.as_str(), "dropped");
    }

    #[test]
    fn test_outcome_as_str_late_tick() {
        assert_eq!(
            AggregatorSealAuditOutcome::LateTickDiscarded.as_str(),
            "late_tick"
        );
    }

    #[test]
    fn test_outcome_variants_pairwise_distinct() {
        // The future Grafana SYMBOL filter dropdown relies on these
        // strings being unique. Pin them.
        let all = [
            AggregatorSealAuditOutcome::Ok.as_str(),
            AggregatorSealAuditOutcome::Dropped.as_str(),
            AggregatorSealAuditOutcome::LateTickDiscarded.as_str(),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "outcome strings must be pairwise distinct");
                }
            }
        }
    }

    // ── Construction ────────────────────────────────────────────

    #[test]
    fn test_for_test_constructor_creates_disconnected_writer() {
        let w = AggregatorSealAuditWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending_count(), 0);
        assert_eq!(w.buffer_row_count(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_is_connected_returns_false_in_test_mode() {
        let w = AggregatorSealAuditWriter::for_test();
        assert!(!w.is_connected());
    }

    #[test]
    fn test_buffer_bytes_returns_empty_slice_initially() {
        let w = AggregatorSealAuditWriter::for_test();
        assert!(w.buffer_bytes().is_empty());
    }

    #[test]
    fn test_buffer_byte_count_starts_at_zero() {
        let w = AggregatorSealAuditWriter::for_test();
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_new_lazy_connects_when_questdb_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 1, // closed
        };
        let w = AggregatorSealAuditWriter::new(&cfg)
            .expect("constructor must not return Err on disconnect");
        assert!(!w.is_connected());
        assert_eq!(w.pending_count(), 0);
    }

    // ── Append ──────────────────────────────────────────────────

    #[test]
    fn test_append_increments_pending_count() {
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        assert_eq!(w.pending_count(), 1);
        append_minimal(&mut w, 25, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        assert_eq!(w.pending_count(), 2);
    }

    #[test]
    fn test_append_increments_buffer_row_count() {
        let mut w = AggregatorSealAuditWriter::for_test();
        for sid in [13, 25, 51] {
            append_minimal(&mut w, sid, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        }
        assert_eq!(w.buffer_row_count(), 3);
    }

    #[test]
    fn test_append_writes_table_name_to_buffer() {
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("aggregator_seal_audit"),
            "expected table name in bytes: {s}"
        );
    }

    #[test]
    fn test_append_writes_segment_symbol_to_buffer() {
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        append_minimal(&mut w, 25, EXCHANGE_SEGMENT_NSE_EQ, TfIndex::M1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("IDX_I"), "IDX_I missing");
        assert!(s.contains("NSE_EQ"), "NSE_EQ missing");
    }

    #[test]
    fn test_append_writes_timeframe_symbol_for_each_tf() {
        // For each TF the symbol value must end up in the wire bytes.
        for tf in TfIndex::ALL {
            let mut w = AggregatorSealAuditWriter::for_test();
            append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, tf);
            let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
            assert!(
                s.contains(tf.display_name()),
                "TF {tf:?}: timeframe symbol `{}` missing in bytes",
                tf.display_name()
            );
        }
    }

    #[test]
    fn test_append_writes_outcome_symbol_for_each_variant() {
        // Each outcome variant must end up in the wire bytes.
        for outcome in [
            AggregatorSealAuditOutcome::Ok,
            AggregatorSealAuditOutcome::Dropped,
            AggregatorSealAuditOutcome::LateTickDiscarded,
        ] {
            let mut w = AggregatorSealAuditWriter::for_test();
            w.append(
                1_716_000_900_000_000_000,
                1_716_000_000_000_000,
                13,
                EXCHANGE_SEGMENT_IDX_I,
                TfIndex::M1,
                1_716_000_900_000_000_000,
                1,
                0,
                0,
                outcome,
            )
            .expect("ok");
            let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
            assert!(
                s.contains(outcome.as_str()),
                "outcome `{}` missing in bytes",
                outcome.as_str()
            );
        }
    }

    #[test]
    fn test_append_writes_security_id_column() {
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("security_id=13i"));
    }

    #[test]
    fn test_append_writes_count_columns() {
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("seals_emitted="));
        assert!(s.contains("seals_dropped="));
        assert!(s.contains("late_ticks_discarded="));
    }

    #[test]
    fn test_append_isolates_segments_in_wire_bytes_for_i_p1_11() {
        // I-P1-11: same security_id, different segments → both
        // segment SYMBOLs in wire bytes.
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_NSE_EQ, TfIndex::M1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("IDX_I"));
        assert!(s.contains("NSE_EQ"));
        assert_eq!(w.buffer_row_count(), 2);
    }

    #[test]
    fn test_repeated_appends_accumulate_buffer_bytes() {
        let mut w = AggregatorSealAuditWriter::for_test();
        let mut prev = 0;
        for i in 0..5 {
            append_minimal(&mut w, 13 + i, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
            let now = w.buffer_byte_count();
            assert!(now > prev, "buffer must grow on each append");
            prev = now;
        }
        assert_eq!(w.buffer_row_count(), 5);
    }

    // ── Flush ───────────────────────────────────────────────────

    #[test]
    fn test_flush_returns_err_when_disconnected() {
        let mut w = AggregatorSealAuditWriter::for_test();
        let result = w.flush();
        assert!(result.is_err());
    }

    #[test]
    fn test_flush_returns_err_when_disconnected_with_pending_rows() {
        let mut w = AggregatorSealAuditWriter::for_test();
        append_minimal(&mut w, 13, EXCHANGE_SEGMENT_IDX_I, TfIndex::M1);
        assert_eq!(w.pending_count(), 1);
        let result = w.flush();
        assert!(result.is_err());
        // Rescue invariant: pending_count preserved so the future
        // writer task can re-route to spill.
        assert_eq!(w.pending_count(), 1);
    }
}
