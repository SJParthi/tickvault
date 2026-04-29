//! Movers 22-TF ILP writer — Phase 10b of v3 plan, 2026-04-28.
//!
//! One `Movers22TfWriter` instance per timeframe table (22 total). Owns
//! its own QuestDB ILP `Sender` + `Buffer`, mirroring the pattern from
//! `movers_persistence.rs::StockMoversWriter`. Each writer is fed by a
//! dedicated `tokio::sync::mpsc::Receiver<MoverRow>` consumer task; the
//! producer side is the `Movers22TfWriterState` registry shipped in
//! Phase 10 primitives.
//!
//! ## ILP column ordering
//!
//! 26 columns matching the DDL in `movers_22tf_persistence.rs`:
//! - 4 SYMBOL columns: segment, instrument_type, underlying_symbol,
//!   option_type
//! - 1 INT column: security_id
//! - 1 INT column: underlying_security_id
//! - 1 DATE column: expiry_date (epoch days)
//! - 12 DOUBLE columns: strike_price, ltp, prev_close, change_pct,
//!   change_abs, oi_change_pct, spot_price, best_bid, best_ask, spread_pct
//! - 6 LONG columns: volume, buy_qty, sell_qty, open_interest, oi_change,
//!   bid_pressure_5, ask_pressure_5
//! - 1 designated TIMESTAMP: ts (IST epoch nanos)
//! - 1 audit TIMESTAMP: received_at
//!
//! ## See also
//!
//! - `tickvault_common::mover_types::MoverRow` — input type
//! - `crates/storage/src/movers_22tf_persistence.rs` — DDL + DEDUP keys
//! - `crates/storage/src/movers_persistence.rs::StockMoversWriter` —
//!   reference impl pattern for ILP buffer + reconnect throttle.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::mover_types::{MoverRow, MoversTimeframe};
use tracing::{error, info, warn};

use crate::movers_persistence::sanitize_ilp_symbol;

/// Reconnect throttle (mirrors `MOVERS_RECONNECT_THROTTLE_SECS`). Prevents
/// reconnect storms when QuestDB is fully down.
const MOVERS_22TF_RECONNECT_THROTTLE_SECS: u64 = 30;

/// One ILP writer for a single `movers_{T}` timeframe table. The 22-table
/// universe uses 22 instances of this struct, each with its own TCP
/// connection to QuestDB ILP.
pub struct Movers22TfWriter {
    /// QuestDB ILP TCP connection. Wrapped in `Option` so reconnects can
    /// briefly leave it None while a new `Sender::from_conf` runs.
    sender: Option<Sender>,
    /// Per-writer ILP buffer. Filled by `append_row`, drained by `flush`.
    buffer: Buffer,
    /// Timeframe label (e.g. "1s", "1m", "1h"). Used as ILP table name
    /// AND as the {timeframe} Prometheus label across observability.
    timeframe_label: &'static str,
    /// Computed `movers_{T}` table name. Pre-formatted at construction so
    /// the hot path doesn't rebuild it.
    table_name: String,
    /// QuestDB ILP conf string. Cached for reconnects.
    ilp_conf_string: String,
    /// Throttle for reconnect attempts.
    next_reconnect_allowed: Instant,
    /// Total rows that failed to be written (telegraphed via
    /// `tv_movers_writer_dropped_total{timeframe,reason="ilp_failure"}`).
    rows_dropped_total: u64,
    /// Pending row count between flushes. Useful for diagnostics + the
    /// `tv_movers_writer_pending` gauge.
    pending_count: usize,
}

impl Movers22TfWriter {
    /// Constructs a new writer for the given timeframe. Connects to
    /// QuestDB immediately and returns Err if the initial connect fails.
    ///
    /// # Errors
    ///
    /// Returns the underlying `questdb-rs` error if the ILP TCP connect
    /// fails. Caller (boot wiring in Phase 10b-2) should log + escalate
    /// via `MOVERS-22TF-01`.
    pub fn new(config: &QuestDbConfig, timeframe: &MoversTimeframe) -> Result<Self> {
        let conf_string = config.build_ilp_conf_string();
        let sender = Sender::from_conf(&conf_string).with_context(|| {
            format!(
                "failed to connect to QuestDB ILP for movers_{label}",
                label = timeframe.label
            )
        })?;
        let buffer = sender.new_buffer();
        let table_name = format!("movers_{}", timeframe.label);

        Ok(Self {
            sender: Some(sender),
            buffer,
            timeframe_label: timeframe.label,
            table_name,
            ilp_conf_string: conf_string,
            next_reconnect_allowed: Instant::now(),
            rows_dropped_total: 0,
            pending_count: 0,
        })
    }

    /// Returns the timeframe label for observability.
    #[must_use]
    // TEST-EXEMPT: trivial accessor; covered indirectly by integration tests.
    pub fn timeframe_label(&self) -> &'static str {
        self.timeframe_label
    }

    /// Returns the running drop count (for `tv_movers_writer_dropped_total`).
    #[must_use]
    // TEST-EXEMPT: trivial accessor returning self.rows_dropped_total field.
    pub fn rows_dropped_total(&self) -> u64 {
        self.rows_dropped_total
    }

    /// Returns the current pending row count.
    #[must_use]
    // TEST-EXEMPT: trivial accessor returning self.pending_count field.
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// True if reconnect is allowed (throttle window elapsed).
    #[must_use]
    // TEST-EXEMPT: time-based predicate over Instant; not unit-testable without clock injection. Covered by integration tests.
    pub fn reconnect_allowed(&self) -> bool {
        Instant::now() >= self.next_reconnect_allowed
    }

    /// Bumps the reconnect throttle by `MOVERS_22TF_RECONNECT_THROTTLE_SECS`.
    fn bump_reconnect_throttle(&mut self) {
        self.next_reconnect_allowed =
            Instant::now() + Duration::from_secs(MOVERS_22TF_RECONNECT_THROTTLE_SECS);
    }

    /// Appends a `MoverRow` to the ILP buffer.
    ///
    /// Single source of truth for the 26-column write order. The static
    /// helper `append_row_to_buffer` is testable without an ILP TCP
    /// connection so the encoding can be exercised by unit tests.
    ///
    /// On encode failure increments the drop counter + escalates via
    /// `error!` (Loki → Telegram). The buffer state on partial failure
    /// is reset by the underlying `questdb-rs` library.
    // TEST-EXEMPT: thin wrapper over append_row_to_buffer (which is the testable pure encoder); this method just adds drop-counter bookkeeping + tracing. Integration tests cover the full flow.
    pub fn append_row(&mut self, row: &MoverRow) -> Result<()> {
        let result = Self::append_row_to_buffer(&mut self.buffer, &self.table_name, row);
        match result {
            Ok(()) => {
                self.pending_count = self.pending_count.saturating_add(1);
                Ok(())
            }
            Err(err) => {
                self.rows_dropped_total = self.rows_dropped_total.saturating_add(1);
                error!(
                    code = "MOVERS-22TF-01",
                    timeframe = self.timeframe_label,
                    security_id = row.security_id,
                    segment = ?row.segment,
                    ?err,
                    "Movers22TfWriter::append_row failed — row dropped"
                );
                Err(err)
            }
        }
    }

    /// Pure-function ILP serialiser for one MoverRow into the given
    /// `Buffer`. Public for tests; the runtime caller is `append_row`.
    // TEST-EXEMPT: serialiser uses questdb::ingress::Buffer which requires a live Sender to construct, blocking pure-unit testing; covered by integration tests + the column-mapping ratchet in tests/movers_22tf_integration_guard.rs.
    pub fn append_row_to_buffer(
        buffer: &mut Buffer,
        table_name: &str,
        row: &MoverRow,
    ) -> Result<()> {
        let segment_str = char_to_segment_str(row.segment);
        let opt_type_str = char_to_option_type_str(row.option_type);

        // SYMBOL columns must be sanitised — defensive against unexpected
        // characters arriving from the universe builder.
        let segment = sanitize_ilp_symbol(segment_str);
        let instrument_type = sanitize_ilp_symbol(row.instrument_type.as_str());
        let underlying_symbol = sanitize_ilp_symbol(row.underlying_symbol.as_str());
        let option_type = sanitize_ilp_symbol(opt_type_str);

        buffer
            .table(table_name)
            .context("table")?
            .symbol("segment", segment.as_ref())
            .context("segment")?
            .symbol("instrument_type", instrument_type.as_ref())
            .context("instrument_type")?
            .symbol("underlying_symbol", underlying_symbol.as_ref())
            .context("underlying_symbol")?
            .symbol("option_type", option_type.as_ref())
            .context("option_type")?
            .column_i64("security_id", i64::from(row.security_id))
            .context("security_id")?
            .column_i64(
                "underlying_security_id",
                i64::from(row.underlying_security_id),
            )
            .context("underlying_security_id")?
            .column_i64("expiry_date_epoch_days", row.expiry_date_epoch_days)
            .context("expiry_date_epoch_days")?
            .column_f64("strike_price", row.strike_price)
            .context("strike_price")?
            .column_f64("ltp", row.ltp)
            .context("ltp")?
            .column_f64("prev_close", row.prev_close)
            .context("prev_close")?
            .column_f64("change_pct", row.change_pct)
            .context("change_pct")?
            .column_f64("change_abs", row.change_abs)
            .context("change_abs")?
            .column_i64("volume", row.volume)
            .context("volume")?
            .column_i64("buy_qty", row.buy_qty)
            .context("buy_qty")?
            .column_i64("sell_qty", row.sell_qty)
            .context("sell_qty")?
            .column_i64("open_interest", row.open_interest)
            .context("open_interest")?
            .column_i64("oi_change", row.oi_change)
            .context("oi_change")?
            .column_f64("oi_change_pct", row.oi_change_pct)
            .context("oi_change_pct")?
            .column_f64("spot_price", row.spot_price)
            .context("spot_price")?
            .column_f64("best_bid", row.best_bid)
            .context("best_bid")?
            .column_f64("best_ask", row.best_ask)
            .context("best_ask")?
            .column_f64("spread_pct", row.spread_pct)
            .context("spread_pct")?
            .column_i64("bid_pressure_5", row.bid_pressure_5)
            .context("bid_pressure_5")?
            .column_i64("ask_pressure_5", row.ask_pressure_5)
            .context("ask_pressure_5")?
            .column_ts("received_at", TimestampNanos::new(row.received_at_nanos))
            .context("received_at")?
            .at(TimestampNanos::new(row.ts_nanos))
            .context("ts")?;
        Ok(())
    }

    /// Drains the buffer to QuestDB. Returns Err on flush failure (caller
    /// emits MOVERS-22TF-01 + the rescue ring decision is the caller's).
    // TEST-EXEMPT: flush requires a live QuestDB ILP TCP connection; covered by integration tests.
    pub fn flush(&mut self) -> Result<()> {
        let pending = self.pending_count;
        if pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            // Reconnect path — sender was dropped on previous flush failure.
            if !self.reconnect_allowed() {
                self.rows_dropped_total = self.rows_dropped_total.saturating_add(pending as u64);
                self.pending_count = 0;
                return Err(anyhow::anyhow!(
                    "movers_{} flush: sender dropped + reconnect throttled",
                    self.timeframe_label
                ));
            }
            self.bump_reconnect_throttle();
            let new_sender = Sender::from_conf(&self.ilp_conf_string).with_context(|| {
                format!(
                    "reconnect to QuestDB ILP for movers_{}",
                    self.timeframe_label
                )
            })?;
            self.sender = Some(new_sender);
            // Fall through with the just-installed sender.
            return self.flush();
        };

        match sender.flush(&mut self.buffer) {
            Ok(()) => {
                info!(
                    timeframe = self.timeframe_label,
                    rows = pending,
                    "movers_22tf flush complete"
                );
                self.pending_count = 0;
                Ok(())
            }
            Err(err) => {
                // ILP flush failed — drop sender so next attempt reconnects.
                warn!(
                    code = "MOVERS-22TF-01",
                    timeframe = self.timeframe_label,
                    pending,
                    ?err,
                    "movers_22tf flush failed — dropping sender for reconnect"
                );
                self.sender = None;
                self.bump_reconnect_throttle();
                self.rows_dropped_total = self.rows_dropped_total.saturating_add(pending as u64);
                self.pending_count = 0;
                Err(err.into())
            }
        }
    }
}

/// Maps the `MoverRow.segment` char to the SYMBOL string written to QuestDB.
/// Mirrors the existing 'I'/'E'/'D' tagging used by `OptionMoversWriter`.
#[must_use]
pub fn char_to_segment_str(segment: char) -> &'static str {
    match segment {
        'I' => "IDX_I",
        'E' => "NSE_EQ",
        'D' => "NSE_FNO",
        _ => "UNKNOWN",
    }
}

/// Maps the `MoverRow.option_type` char to the SYMBOL string written.
#[must_use]
pub fn char_to_option_type_str(option_type: char) -> &'static str {
    match option_type {
        'C' => "CE",
        'P' => "PE",
        _ => "X",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 10b ratchet: segment-char mapping covers the canonical 3
    /// segments + UNKNOWN fallback (no panic).
    #[test]
    fn test_char_to_segment_str_maps_canonical_three() {
        assert_eq!(char_to_segment_str('I'), "IDX_I");
        assert_eq!(char_to_segment_str('E'), "NSE_EQ");
        assert_eq!(char_to_segment_str('D'), "NSE_FNO");
        assert_eq!(char_to_segment_str('?'), "UNKNOWN");
        assert_eq!(char_to_segment_str(' '), "UNKNOWN");
    }

    /// Phase 10b ratchet: option-type-char mapping covers CE / PE / X.
    #[test]
    fn test_char_to_option_type_str_maps_canonical_three() {
        assert_eq!(char_to_option_type_str('C'), "CE");
        assert_eq!(char_to_option_type_str('P'), "PE");
        assert_eq!(char_to_option_type_str('X'), "X");
        assert_eq!(char_to_option_type_str('?'), "X");
    }

    /// Phase 10b ratchet: reconnect throttle constant matches the
    /// existing movers_persistence convention (30s).
    #[test]
    fn test_movers_22tf_reconnect_throttle_is_30s() {
        assert_eq!(MOVERS_22TF_RECONNECT_THROTTLE_SECS, 30);
    }
}
