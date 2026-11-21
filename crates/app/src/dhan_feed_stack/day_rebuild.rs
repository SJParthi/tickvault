//! The past-day refold behind `tickvault day-repair` (daily-universe Quote 63,
//! plan ITEM UTC-REPAIR): one held IST day rebuilt from its raw frames.
//!
//! A child of `dhan_feed_stack` because it drives the same private fold the
//! WAL replay drives (`refold_one_tick`, the backup dedup, the aux rows, the
//! seal routing); the live path is untouched and
//! [`super::refold_wal_frames`] is not edited. Two differences from that
//! function, and they are why this exists:
//!
//! - the aggregator's event-time watermark is seeded to the START OF THE
//!   REPAIRED DAY, never to today (`refold_wal_frames` seeds today from the
//!   wall clock, which would refuse every candle of a past day as
//!   `stale_trading_day` and leave the candle tables empty);
//! - the day ends with the catch-up seal past the close, the day-boundary
//!   seal and the replay hand-over, all while the fold is still the repair's
//!   own (never `withhold_open_buckets`, never the live capture start "now").
//!
//! No depth sink is installed by the repair (depth rows were correct and are
//! not dropped), so a depth frame is counted and skipped, and a Full packet's
//! inline levels are not re-written.
//!
//! The correction itself needs nothing here: `dispatch_frame` applies the
//! permanent UTC trade-time rule from each frame's WAL receipt
//! (`tickvault_storage::ltt_correction::corrected_ltt`).
//!
//! O(frames + packets) per segment, offline (the app is stopped), never on a
//! live drain. Deleted with the rest of the one-off tool after the run.

use super::*;

/// Seals the end of a past-day refold produced, beyond the per-tick seals
/// the fold already counted on the ingest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PastDayClose {
    /// Bars the catch-up seal past the close emitted, rescued or lost.
    pub catch_up_bars: u64,
    /// Bars the day-boundary seal emitted.
    pub close_bars: u64,
    /// Buckets the replay hand-over tainted (none expected after the close
    /// seals: nothing is still open).
    pub tainted: u64,
    /// Partial bars the replay suppressed over the whole day.
    pub suppressed: u64,
}

impl LiveIngest {
    /// Prepares a fresh ingest to refold one whole past IST day.
    ///
    /// `day_start_ist_secs` is 00:00 IST of the day as fold seconds (epoch
    /// seconds plus the IST offset); `capture_start_ist_secs` is the receipt
    /// of the day's first main-feed frame, the instant the live process began
    /// capturing that morning. Call once, before the first segment. O(1)
    /// apart from the backup history read.
    pub(crate) fn begin_past_day_refold(
        &mut self,
        day_start_ist_secs: u32,
        capture_start_ist_secs: u32,
    ) {
        self.replaying_wal = true;
        self.aux_repeat.clear();
        self.aggregator.set_replay_mode(true);
        self.aggregator.seed_watermark_at_least(day_start_ist_secs);
        self.aggregator
            .set_live_capture_start(capture_start_ist_secs);
        // The live drain folded one copy of each backed-up packet; so must
        // the rebuild (scope lock 2026-10-02).
        self.load_replay_backup();
    }

    /// Folds one segment's frames of the repaired day.
    ///
    /// `frames` are `(frame_seq, wal_received_at_nanos, endpoint, bytes)` in
    /// write order, already filtered to the repaired day by the caller;
    /// `gaps` holds ascending indexes of frames that follow a gap (a restart
    /// or a damaged stretch on the day). The rows go through the same
    /// `ingest_tick_at` / `ingest_non_tick_at` the live drain uses, with the
    /// recorded receipt, so `capture_seq` and every DEDUP key are what live
    /// wrote. O(frames + packets).
    pub(crate) fn refold_past_day_segment(
        &mut self,
        frames: &[(u64, i64, WalEndpoint, bytes::Bytes)],
        gaps: &[usize],
        out: &mut WalRefoldOutcome,
    ) {
        // The tick-gap detector's liveness clock: deliberately "now", as in
        // `refold_wal_frames` (never an input to bucketing).
        let recv_millis =
            u64::try_from(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) / 1_000_000)
                .unwrap_or(0);
        let mut gap_cursor = 0usize;
        for (frame_idx, (frame_seq, receipt, endpoint, bytes)) in frames.iter().enumerate() {
            self.frame_receipt_is_wal_stamp =
                crate::tick_quote_route::receipt_is_wal_stamp(*receipt);
            self.frame_wal_unbacked = false;
            let mut gap_marked_here = false;
            while let Some(&gap_idx) = gaps.get(gap_cursor) {
                if gap_idx > frame_idx {
                    break;
                }
                if gap_idx == frame_idx && !gap_marked_here {
                    self.mark_replay_gap();
                    out.gaps_marked = out.gaps_marked.saturating_add(1);
                    gap_marked_here = true;
                }
                gap_cursor += 1;
            }
            // The live size trigger, so the synchronous writer never holds a
            // whole segment's rows (the 2026-09-02 4 GB buffer).
            if self.pending_rows() >= FLUSH_ROW_THRESHOLD {
                blocking_flush(|| self.flush_ticks());
            }
            match endpoint {
                WalEndpoint::Depth20 | WalEndpoint::Depth200 => {
                    out.depth_frames = out.depth_frames.saturating_add(1);
                    continue;
                }
                WalEndpoint::MainFeed | WalEndpoint::OrderUpdate => {}
            }
            if is_depth_socket_frame(bytes) {
                out.depth_frames = out.depth_frames.saturating_add(1);
                continue;
            }
            let backup_covers = self
                .replay_backup
                .as_mut()
                .is_some_and(|r| r.select(*receipt));
            self.refold_past_day_packets(
                bytes,
                *frame_seq,
                *receipt,
                backup_covers,
                recv_millis,
                out,
            );
        }
    }

    /// The packet walk of one main-feed frame: the same boundaries, recovery
    /// and per-variant handling as `refold_wal_frames`. O(packets).
    fn refold_past_day_packets(
        &mut self,
        bytes: &bytes::Bytes,
        frame_seq: u64,
        receipt: i64,
        backup_covers: bool,
        recv_millis: u64,
        out: &mut WalRefoldOutcome,
    ) {
        let mut offset = 0usize;
        let mut packets = 0u32;
        while offset < bytes.len() {
            let Some(len) =
                tickvault_core::parser::dispatcher::main_feed_packet_len(&bytes[offset..])
            else {
                if let Some(next) = unknown_packet_skip(bytes, offset) {
                    out.unknown_skipped = out.unknown_skipped.saturating_add(1);
                    offset = next;
                    packets = packets.saturating_add(1);
                    continue;
                }
                out.unparseable = out.unparseable.saturating_add(1);
                out.abandoned_bytes = out
                    .abandoned_bytes
                    .saturating_add(bytes.len().saturating_sub(offset) as u64);
                break;
            };
            let end = offset.saturating_add(len);
            if end > bytes.len() {
                out.unparseable = out.unparseable.saturating_add(1);
                out.abandoned_bytes = out
                    .abandoned_bytes
                    .saturating_add(bytes.len().saturating_sub(offset) as u64);
                break;
            }
            let packet = &bytes[offset..end];
            match dispatch_frame(packet, receipt) {
                Ok(ParsedFrame::Tick(ref t) | ParsedFrame::TickWithDepth(ref t, _))
                    if backup_covers && self.replay_backup_drops_tick(t, packet) =>
                {
                    out.backup_dropped = out.backup_dropped.saturating_add(1);
                }
                Ok(
                    ParsedFrame::PreviousClose {
                        security_id,
                        exchange_segment_code,
                        ..
                    }
                    | ParsedFrame::OiUpdate {
                        security_id,
                        exchange_segment_code,
                        ..
                    },
                ) if backup_covers
                    && self.replay_backup_drops_aux(security_id, exchange_segment_code, packet) =>
                {
                    out.non_tick = out.non_tick.saturating_add(1);
                    out.backup_dropped = out.backup_dropped.saturating_add(1);
                }
                Ok(ParsedFrame::TickWithDepth(tick, _) | ParsedFrame::Tick(tick)) => {
                    refold_one_tick(self, &tick, frame_seq, packets, recv_millis, out);
                }
                Ok(non_tick) => {
                    out.non_tick = out.non_tick.saturating_add(1);
                    let _kept =
                        self.ingest_non_tick_at(&non_tick, packet, frame_seq, packets, receipt);
                }
                Err(_) => {
                    out.undecodable = out.undecodable.saturating_add(1);
                }
            }
            packets = packets.saturating_add(1);
            offset = end;
        }
    }

    /// Flushes the rows still held (a synchronous round trip off the async
    /// worker; a failure rescues them to the tick spill tier, which the
    /// repair's verify then fails on). Returns the rows handed over.
    // TEST-EXEMPT: covered by test_begin_past_day_refold_seeds_the_day_not_today
    pub(crate) fn flush_past_day_rows(&mut self) -> u64 {
        blocking_flush(|| self.flush_ticks())
    }

    /// `(tick_quotes rows, feed_aux_packets rows)` this ingest's writer
    /// appended. O(1).
    #[must_use]
    pub(crate) const fn past_day_row_counts(&self) -> (u64, u64) {
        (
            self.writer.quote_rows_appended(),
            self.writer.aux_rows_appended(),
        )
    }

    /// Ends the past-day refold: flushes the rows, seals every bucket up to
    /// `cutoff_ist_secs` (past the day's last bucket), runs the day-boundary
    /// seal, then the replay hand-over, all routed to the seal writer the
    /// same way the live catch-up seal routes them. O(slots × `TF_COUNT`).
    pub(crate) fn finish_past_day_refold(&mut self, cutoff_ist_secs: u32) -> PastDayClose {
        blocking_flush(|| self.flush_ticks());
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let mut tally = SealTally::default();
        let catch_up = self.aggregator.catch_up_seal_all(
            cutoff_ist_secs,
            |feed, security_id, segment_code, tf, state| {
                route_catch_up_seal(
                    sender,
                    feed,
                    security_id,
                    segment_code,
                    tf,
                    state,
                    &mut tally,
                );
            },
        );
        let close = self
            .aggregator
            .force_seal_all(|feed, security_id, segment_code, tf, state| {
                route_catch_up_seal(
                    sender,
                    feed,
                    security_id,
                    segment_code,
                    tf,
                    state,
                    &mut tally,
                );
            });
        let (tainted, suppressed) =
            self.aggregator
                .finish_replay(false, |feed, security_id, segment_code, tf, state| {
                    route_catch_up_seal(
                        sender,
                        feed,
                        security_id,
                        segment_code,
                        tf,
                        state,
                        &mut tally,
                    );
                });
        self.apply_seal_tally(tally);
        self.replay_backup = None;
        self.replaying_wal = false;
        PastDayClose {
            catch_up_bars: catch_up as u64,
            close_bars: close as u64,
            tainted,
            suppressed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A main-feed frame of one Ticker packet (16 bytes, code 2).
    fn ticker_frame(security_id: u32, segment: u8, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut f = vec![0u8; 16];
        f[0] = 2;
        f[1..3].copy_from_slice(&16u16.to_le_bytes());
        f[3] = segment;
        f[4..8].copy_from_slice(&security_id.to_le_bytes());
        f[8..12].copy_from_slice(&ltp.to_le_bytes());
        f[12..16].copy_from_slice(&ltt.to_le_bytes());
        f
    }

    #[test]
    fn test_begin_past_day_refold_seeds_the_day_not_today() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = 1_795_113_000u32.saturating_add(19_800);
        ingest.begin_past_day_refold(day_start, day_start + 9 * 3_600);
        assert_eq!(ingest.aggregator.watermark_secs(), day_start);
        assert!(ingest.replaying_wal);
        assert_eq!(ingest.past_day_row_counts(), (0, 0));
        assert_eq!(ingest.flush_past_day_rows(), 0);
    }

    #[test]
    fn test_finish_past_day_refold_ends_replay_and_clears_the_backup() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = 1_795_113_000u32.saturating_add(19_800);
        ingest.begin_past_day_refold(day_start, day_start + 9 * 3_600);
        let close = ingest.finish_past_day_refold(day_start + 16 * 3_600 + 300);
        assert_eq!(close.tainted, 0);
        assert!(!ingest.replaying_wal);
        assert!(ingest.replay_backup.is_none());
    }

    #[test]
    fn test_refold_past_day_segment_counts_depth_frames_and_marks_gaps() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = 1_795_113_000u32.saturating_add(19_800);
        ingest.begin_past_day_refold(day_start, day_start + 9 * 3_600);
        let receipt = 1_795_113_000_000_000_000i64 + 4 * 3_600 * 1_000_000_000;
        let frames = vec![
            (
                1u64,
                receipt,
                WalEndpoint::Depth20,
                bytes::Bytes::from(vec![0u8; 12]),
            ),
            (
                2u64,
                receipt,
                WalEndpoint::MainFeed,
                bytes::Bytes::from(ticker_frame(13, 0, 24_000.0, 1_795_115_000)),
            ),
        ];
        let mut out = WalRefoldOutcome::default();
        ingest.refold_past_day_segment(&frames, &[1], &mut out);
        assert_eq!(out.depth_frames, 1);
        assert_eq!(out.gaps_marked, 1);
        assert_eq!(
            out.refolded + out.lost + out.refused_wrong_day + out.undecodable + out.non_tick,
            1,
            "the one ticker packet is accounted for exactly once"
        );
    }
}
