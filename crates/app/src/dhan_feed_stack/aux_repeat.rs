//! Operator Quote 61 (2026-11-14): the wrong-day aux row (`no_trade_today`) is
//! written only when it differs from the contract's previous written row.
//!
//! A child module so `dhan_feed_stack.rs` (size-pinned) carries only the call.
//! The decision itself is `tickvault_storage::feed_aux_repeat::AuxRepeatFilter`;
//! this is the drain's use of it. The live drain and the WAL replay both reach
//! it through `LiveIngest::ingest_tick_at`, so a replayed frame meets the same
//! rule; `refold_wal_frames` clears the filter at the start AND the end of
//! every pass, so a replay never inherits the live drain's last rows and the
//! live drain never inherits a replayed (possibly older) row, and inside a pass
//! every replay gap goes through [`LiveIngest::mark_replay_gap`], so the first
//! row per contract after a gap is written. A contract the main feed carries
//! on two sockets (the backup copy) is never skipped (`carried_twice`), and
//! neither is a row of a frame the WAL refused (`frame_wal_unbacked`, 2026-11-14
//! audit fix): the skip rests on the WAL keeping every packet.

use tickvault_common::tick_types::ParsedTick;
use tickvault_core::websocket::pool_supervisor::live_shed::main_feed_live_shed_total;
use tickvault_storage::candle_contract_labels::candle_contract_labels_generation;
use tickvault_storage::feed_aux_persistence::{AuxPacketRow, wrong_day_kind};
use tickvault_storage::feed_aux_repeat::{AuxRepeatFilter, AuxRepeatVerdict};
use tickvault_storage::tick_conservation_ledger::ledger;

use super::LiveIngest;

/// The gap epoch the production filter reads: main-feed frames shed before
/// the drain (the WAL keeps them and a later replay writes their rows), staged
/// rows that did not reach QuestDB (failed flush, rescue or spill), and the
/// contract-name table's publish generation (a row's `contract` name is read
/// from that table when it is staged, so after a publish the next row is
/// written with the new name). All three only grow, so the sum moves whenever
/// any does, and every row remembered before the move stops arming a skip.
/// Three atomic loads, O(1), no allocation.
pub(super) fn gap_epoch() -> u64 {
    main_feed_live_shed_total()
        .wrapping_add(ledger().rows_not_landed())
        .wrapping_add(candle_contract_labels_generation())
}

/// The drain's filter, reading [`gap_epoch`]. Pre-sizes the map once, at
/// construction.
pub(super) fn production_filter() -> AuxRepeatFilter {
    AuxRepeatFilter::with_gap_epoch(gap_epoch)
}

/// The `feed_aux_packets` row for a wrong-day `tick`, labelled by its receipt
/// (`wrong_day_kind`: `connect_snapshot` before the cutover, `no_trade_today`
/// from it), so a replayed frame gets the label the live drain gave it.
/// `None` for an id that does not fit the column. Pure, O(1).
fn no_trade_today_row(tick: &ParsedTick, capture_seq: i64) -> Option<AuxPacketRow> {
    AuxPacketRow::from_tick(wrong_day_kind(tick.received_at_nanos), tick, capture_seq)
}

impl LiveIngest {
    /// Writes the `no_trade_today` row for `tick` unless it repeats the
    /// contract's last written row exactly, with no gap since (skipped and
    /// counted). A row of a frame with no WAL copy is never skipped. A row is
    /// remembered once it is staged; a staged row that later fails to land
    /// moves the gap epoch, so the next repeat is written.
    /// O(1) average, no allocation.
    pub(super) fn append_no_trade_today(&mut self, tick: &ParsedTick, capture_seq: i64) {
        let Some(row) = no_trade_today_row(tick, capture_seq) else {
            return;
        };
        let key = (tick.security_id, tick.exchange_segment_code);
        let verdict = if self.carried_twice(key) {
            self.aux_repeat.check_never_skip(key, &row)
        } else if self.frame_wal_unbacked {
            // The skip rests on the WAL keeping the packet; this frame has no
            // WAL copy, so its row is always written (2026-11-14 audit fix).
            self.aux_repeat.check_always_write(key, &row)
        } else {
            self.aux_repeat.check(key, &row)
        };
        if let AuxRepeatVerdict::Write(values) = verdict
            && self.append_aux_row(&row)
        {
            self.aux_repeat.note_written(key, values);
        }
    }

    /// Whether the main feed carries `key` on two sockets: in the live backup
    /// set in force, or in the replay's backup publication for this frame.
    /// Such a contract's wrong-day rows are never skipped (Quote 61 review,
    /// 2026-11-14): a ring shed on one socket is counted on that socket's own
    /// read task and is not ordered against the other socket's frames, so a
    /// repeat from the second socket could be judged before the shed moves
    /// the gap epoch, and the stored rows would end on the shed packet's value
    /// with nothing counting it. The replay checks its own publication so a
    /// replayed day writes the same rows. At most two O(1) probes, no
    /// allocation.
    fn carried_twice(&mut self, key: (u64, u8)) -> bool {
        self.backup.holds(key.0, key.1)
            || self
                .replay_backup
                .as_mut()
                .is_some_and(|r| r.dedup_mut().holds(key.0, key.1))
    }

    /// Marks a WAL replay gap (frames before this one were not re-read): on
    /// the candle fold (plan item 47, no volume baseline carries across) and
    /// on the aux repeat filter (Quote 61 review, 2026-11-14: live may have
    /// stored a different row for a contract in the skipped frames, so the
    /// first row after the gap is written, never skipped against the row
    /// before it). O(1) each, no allocation.
    pub(super) fn mark_replay_gap(&mut self) {
        self.aggregator.mark_replay_gap();
        self.aux_repeat.mark_gap();
    }
}

#[cfg(test)]
#[path = "aux_repeat_tests.rs"]
mod tests;
