//! The ILP write functions as the frame drain called them before plan item
//! PF-4 (2026-11-06): each reads the GLOBAL contract-name table at the moment
//! it writes. Since PF-4 production stages rows and the encoding threads call
//! `write_tick_line_with_labels` / `append_aux_row_with_labels` with the
//! table pinned on the drain. These forms are kept, test-only, for the
//! byte-identity tests and the pre-PF-4 oracle (`tick_stage_oracle_tests`).
//! The bodies are the old bodies: one table load, then the shared layout.

use super::*;

/// Writes one `ticks` line, reading the global name table now.
pub(super) fn write_tick_line(
    buffer: &mut TickRowBuffer,
    feed: &'static str,
    row: &TickRow,
) -> Result<()> {
    write_tick_line_into(buffer, TICKS_TABLE, feed, row, None)
}

/// Writes one line of the `ticks` columns into `table`, reading the global
/// name table now (`exchange_timestamp` written when given: `tick_quotes`).
pub(super) fn write_tick_line_into(
    buffer: &mut TickRowBuffer,
    table: &'static str,
    feed: &'static str,
    row: &TickRow,
    exchange_ts_ist_nanos: Option<i64>,
) -> Result<()> {
    let labels = crate::candle_contract_labels::candle_contract_labels();
    write_tick_line_with_labels(buffer, table, feed, row, exchange_ts_ist_nanos, &labels)
}

/// Appends one `feed_aux_packets` row, reading the global name table now.
pub(super) fn append_aux_row<B: IlpLine>(
    buffer: &mut B,
    feed: &str,
    row: &AuxPacketRow,
) -> Result<()> {
    let labels = crate::candle_contract_labels::candle_contract_labels();
    crate::feed_aux_persistence::append_aux_row_with_labels(buffer, feed, row, &labels)
}
