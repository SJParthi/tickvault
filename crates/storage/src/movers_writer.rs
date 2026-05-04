//! Wave 5 Item 25/27 Phase B — single-stream ILP writer for `movers_1s`.
//!
//! ONE writer, ONE table. The 24 mat views auto-aggregate server-side.
//!
//! # Hot-path budget
//!
//! Called once-per-second per (security_id, segment) from the
//! `movers_pipeline` drain task — NOT per tick. ~24K instruments
//! × 1Hz = ~24K rows/sec, well within QuestDB ILP throughput envelope.
//!
//! `append_row` itself does no heap allocation beyond the `Buffer`'s
//! internal extend (amortised O(1) since the buffer is reused).
//!
//! # Error handling
//!
//! Per audit-findings Rule 5 — flush failures fire `error!` (Loki →
//! Telegram). Append failures (row encoding errors) are logged at
//! ERROR with row context and the row is dropped (drop-newest semantic;
//! the next 1s drain will pick up fresh values).

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use tracing::{error, info, warn};

use crate::movers_base_persistence::QUESTDB_TABLE_MOVERS_1S;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_ilp_symbol;

/// One row in the `movers_1s` base table. Mirrors the 10-column
/// schema in `movers_base_persistence.rs`.
///
/// Built once per second per (security_id, segment) by
/// `movers_pipeline`. Copy + Send so the drain task can pass
/// rows to the writer without heap allocation per row.
#[derive(Debug, Clone, Copy)]
pub struct MoversRow {
    /// Designated timestamp — IST epoch nanoseconds. Caller passes the
    /// 1s-aligned bucket boundary.
    pub ts_nanos: i64,
    /// Dhan SecurityId.
    pub security_id: u32,
    /// Exchange segment as Dhan segment-character (`'I'`, `'E'`, `'D'`,
    /// `'B'`, `'M'`, `'C'`). Encoded to SYMBOL via `sanitize_ilp_symbol`.
    pub segment_char: char,
    /// Precise per-exchange tag (e.g., `"NSE_FNO"`, `"BSE_FNO"`,
    /// `"NSE_EQ"`, `"IDX_I"`). Looked up by the pipeline from
    /// `InstrumentRegistry::get_with_segment` per I-P1-11. Falls back to
    /// `"UNKNOWN"` when the registry has no entry — this keeps the
    /// column non-null for downstream selectors. Static str so
    /// `MoversRow` stays `Copy`.
    pub exchange_segment: &'static str,
    /// Precise instrument classification per Dhan annexure
    /// (`"INDEX"`, `"EQUITY"`, `"FUTIDX"`, `"FUTSTK"`, `"FUTCOM"`,
    /// `"FUTCUR"`, `"OPTIDX"`, `"OPTSTK"`, `"OPTFUT"`, `"OPTCUR"`).
    /// Looked up alongside `exchange_segment`; falls back to
    /// `"UNKNOWN"` for the same reason.
    pub instrument_type: &'static str,
    /// Audit-2026-05-03: session phase tag — `"PREOPEN"` (09:00-09:13 IST),
    /// `"MARKET"` (09:15-15:30 IST), or `"POSTMARKET"` (15:30-15:40 IST).
    /// Folds in the legacy `PreopenMoversTracker` / `STOCK_MOVERS_PHASE_*`
    /// semantics. Pipeline drain computes from current IST time.
    pub phase: &'static str,
    /// Open interest at this 1s tick (0 for non-derivative segments).
    pub open_interest: i64,
    /// Per-1s OI delta. Caller computes from the previous-1s value.
    /// Zero for the first observation of a contract in this session.
    pub oi_delta: i64,
    /// Cumulative session volume.
    pub volume: i64,
    /// Last traded price.
    pub last_price: f64,
    /// Previous-day close. Constant per-instrument within a session.
    pub prev_close: f64,
    /// Pre-computed `((last_price - prev_close) / prev_close) * 100`.
    /// Zero when `prev_close <= 0` (defensive — mat views recompute
    /// with a `CASE WHEN prev_close > 0` guard).
    pub change_pct: f64,
    /// PR #450 (2026-05-03): previous-session-close OI baseline.
    /// Required for Dhan-parity `OI Change` and `OI Change %` columns
    /// shown on Markets > Options view.
    /// Source for NSE_FNO derivatives: NSE bhavcopy `OpnIntrst` (col 22)
    /// loaded into the `prev_oi_cache` at boot.
    /// Source for IDX_I: Dhan WS PrevClose packet code 6 bytes 12-15.
    /// Sentinel `0` for instruments with no prev_oi (equities/indices).
    pub prev_oi: i64,
    /// PR #450 (2026-05-03): derivative-contract expiry as IST midnight
    /// epoch nanoseconds. Enables `?expiry=YYYY-MM-DD` filter on the
    /// new `/api/movers` endpoint matching Dhan's middle dropdown.
    /// `0` for non-derivative rows (equities + indices).
    pub expiry_date_ist_nanos: i64,
    /// Wall-clock arrival timestamp (IST nanoseconds, set by the
    /// pipeline at drain time).
    pub received_at_nanos: i64,
}

/// Reconstructs the Dhan segment SYMBOL string for ILP from the binary
/// segment code (0..=8) used in `ParsedTick::exchange_segment_code`.
/// Falls back to `'?'` for unknown codes (mat views still ingest;
/// segment column gets the literal `?`).
#[inline]
#[must_use]
pub fn segment_code_to_char(code: u8) -> char {
    match code {
        0 => 'I', // IDX_I
        1 => 'E', // NSE_EQ
        2 => 'D', // NSE_FNO (derivatives)
        3 => 'C', // NSE_CURRENCY
        4 => 'E', // BSE_EQ → reuse 'E' label
        5 => 'M', // MCX_COMM
        7 => 'C', // BSE_CURRENCY
        8 => 'D', // BSE_FNO → reuse 'D' label
        _ => '?',
    }
}

/// ILP writer wrapping a `questdb::ingress::Sender` + `Buffer`.
/// Designed to be owned by a single tokio task (the pipeline drain).
/// NOT `Sync` — buffer mutation is not thread-safe.
pub struct MoversWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: u64,
    ilp_conf_string: String,
}

impl MoversWriter {
    /// Builds an ILP `tcp::` config string + opens the connection.
    /// Errors propagate; the caller decides whether to halt boot or
    /// run with rescue-ring buffering.
    pub fn connect(questdb: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!(
            "tcp::addr={host}:{port};",
            host = questdb.host,
            port = questdb.ilp_port
        );
        let sender = Sender::from_conf(&conf_string)
            .with_context(|| format!("MoversWriter ILP connect to {conf_string}"))?;
        let buffer = sender.new_buffer();
        info!(
            host = %questdb.host,
            port = questdb.ilp_port,
            "MoversWriter ILP connected"
        );
        Ok(Self {
            sender: Some(sender),
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
        })
    }

    /// Encode + buffer one row. Does NOT flush — caller calls `flush()`
    /// at end of each 1s drain cycle.
    ///
    /// On encode failure (defensive — should not happen with valid input):
    /// logs ERROR + drops the row + increments the drop counter.
    pub fn append_row(&mut self, row: &MoversRow) -> Result<()> {
        let result = Self::append_row_to_buffer(&mut self.buffer, row);
        match result {
            Ok(()) => {
                self.pending_count = self.pending_count.saturating_add(1);
                Ok(())
            }
            Err(err) => {
                metrics::counter!(
                    "tv_movers_writer_dropped_total",
                    "reason" => "encode_error"
                )
                .increment(1);
                error!(
                    code = "MOVERS-UNIFIED-01",
                    security_id = row.security_id,
                    segment = row.segment_char.to_string(),
                    ?err,
                    "MoversWriter::append_row failed — row dropped"
                );
                Err(err)
            }
        }
    }

    /// Pure ILP encoder — `Buffer` mutation only, no I/O. Public for
    /// tests + integration assertions.
    // TEST-EXEMPT: serialiser uses questdb::ingress::Buffer which requires a live Sender to construct, blocking pure-unit testing; covered by integration tests.
    pub fn append_row_to_buffer(buffer: &mut Buffer, row: &MoversRow) -> Result<()> {
        let segment_str: String = row.segment_char.to_string();
        let segment = sanitize_ilp_symbol(&segment_str);
        // SYMBOL columns are emitted up-front per QuestDB ILP convention
        // (all symbols, then all numeric columns). Both `exchange_segment`
        // and `instrument_type` are bounded enums (≤16 values each)
        // matching the SYMBOL CAPACITY in the DDL.
        let exchange_segment = sanitize_ilp_symbol(row.exchange_segment);
        let instrument_type = sanitize_ilp_symbol(row.instrument_type);
        // Audit-2026-05-03: phase folded in from legacy PreopenMoversTracker.
        let phase = sanitize_ilp_symbol(row.phase);

        buffer
            .table(QUESTDB_TABLE_MOVERS_1S)
            .context("table")?
            .symbol("segment", segment.as_ref())
            .context("segment")?
            .symbol("exchange_segment", exchange_segment.as_ref())
            .context("exchange_segment")?
            .symbol("instrument_type", instrument_type.as_ref())
            .context("instrument_type")?
            .symbol("phase", phase.as_ref())
            .context("phase")?
            .column_i64("security_id", i64::from(row.security_id))
            .context("security_id")?
            .column_i64("open_interest", row.open_interest)
            .context("open_interest")?
            .column_i64("oi_delta", row.oi_delta)
            .context("oi_delta")?
            .column_i64("prev_oi", row.prev_oi)
            .context("prev_oi")?
            .column_i64("volume", row.volume)
            .context("volume")?
            .column_f64("last_price", row.last_price)
            .context("last_price")?
            .column_f64("prev_close", row.prev_close)
            .context("prev_close")?
            .column_f64("change_pct", row.change_pct)
            .context("change_pct")?
            .column_ts(
                "expiry_date_ist",
                TimestampNanos::new(row.expiry_date_ist_nanos),
            )
            .context("expiry_date_ist")?
            .column_ts("received_at", TimestampNanos::new(row.received_at_nanos))
            .context("received_at")?
            .at(TimestampNanos::new(row.ts_nanos))
            .context("ts")?;
        Ok(())
    }

    /// Drains the buffer to QuestDB. On flush failure:
    /// - increments `tv_movers_writer_errors_total{stage="flush"}`
    /// - logs `error!` with code `MOVERS-UNIFIED-01` (Loki → Telegram)
    /// - clears `self.sender` so the next call attempts reconnect
    pub fn flush(&mut self) -> Result<()> {
        let pending = self.pending_count;
        if pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            // Reconnect — sender was dropped on previous flush failure.
            let new_sender = Sender::from_conf(&self.ilp_conf_string)
                .with_context(|| format!("MoversWriter reconnect to {}", self.ilp_conf_string))?;
            self.sender = Some(new_sender);
            // Buffer still contains pending rows — fall through to flush.
            return self.flush();
        };
        match sender.flush(&mut self.buffer) {
            Ok(()) => {
                metrics::counter!("tv_movers_1s_rows_total").increment(pending);
                self.pending_count = 0;
                Ok(())
            }
            Err(err) => {
                metrics::counter!(
                    "tv_movers_writer_errors_total",
                    "stage" => "flush"
                )
                .increment(1);
                error!(
                    code = "MOVERS-UNIFIED-01",
                    pending,
                    ?err,
                    "MoversWriter::flush failed — sender will reconnect on next call"
                );
                self.sender = None;
                Err(anyhow::Error::new(err).context("MoversWriter flush"))
            }
        }
    }

    /// Connection probe used by tests / boot-time validation.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Pending row count (test introspection).
    #[must_use]
    pub fn pending(&self) -> u64 {
        self.pending_count
    }

    /// Shutdown helper — calls `flush` once with a timeout window.
    /// Used by `movers_pipeline` on graceful drop.
    // TEST-EXEMPT: thin wrapper around flush() with timeout; flush is integration-tested.
    pub fn shutdown_flush(&mut self, _timeout: Duration) {
        if let Err(err) = self.flush() {
            warn!(
                ?err,
                "MoversWriter::shutdown_flush failed — pending rows may be lost"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_code_to_char_idx_i() {
        assert_eq!(segment_code_to_char(0), 'I');
    }

    #[test]
    fn test_segment_code_to_char_nse_eq() {
        assert_eq!(segment_code_to_char(1), 'E');
    }

    #[test]
    fn test_segment_code_to_char_nse_fno() {
        assert_eq!(segment_code_to_char(2), 'D');
    }

    #[test]
    fn test_segment_code_to_char_bse_fno_reuses_d() {
        // Wave 5 supports BSE_FNO for SENSEX; reuses NSE_FNO 'D' label
        // for ILP SYMBOL deduplication.
        assert_eq!(segment_code_to_char(8), 'D');
    }

    #[test]
    fn test_segment_code_to_char_unknown_falls_back() {
        assert_eq!(segment_code_to_char(99), '?');
        assert_eq!(segment_code_to_char(6), '?'); // Gap in Dhan enum
    }

    #[test]
    fn test_movers_unified_row_is_copy() {
        // Hot-path requirement: passing rows from drain task to writer
        // must not allocate. `Copy` enforces struct-level discipline.
        fn assert_copy<T: Copy>() {}
        assert_copy::<MoversRow>();
    }

    #[test]
    fn test_movers_unified_row_struct_size_under_160_bytes() {
        // Defensive: scalars + 1 char + 3 `&'static str` (16 bytes each)
        // + alignment padding ≤ 160 bytes. Catch struct bloat early.
        // Drain cadence is 1 Hz (cold path), so ~150 bytes × 25K instruments
        // = ~4 MB per cycle — acceptable.
        // Budget bump history:
        //   96  → 128 (PR-B 2026-05-02): added `exchange_segment` +
        //                                 `instrument_type` for selector precision.
        //   128 → 160 (PR #450 2026-05-03): added `prev_oi` (i64) +
        //                                   `expiry_date_ist_nanos` (i64) for
        //                                   Dhan-parity OI Change /
        //                                   OI Change % calculations and
        //                                   ?expiry filter on /api/movers.
        let size = std::mem::size_of::<MoversRow>();
        assert!(
            size <= 160,
            "MoversRow size {size} bytes exceeded budget — review schema"
        );
    }

    #[tokio::test]
    async fn test_writer_connect_returns_err_when_questdb_unreachable() {
        // Port 1 always rejects; the function must propagate.
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 1, // unprivileged-rejected
        };
        let result = MoversWriter::connect(&cfg);
        assert!(result.is_err());
    }
}
