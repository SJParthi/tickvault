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
    /// Partial bars a replay hand-over suppressed. Always 0 since the
    /// rebuild folds in live mode (kept for the report shape).
    pub suppressed: u64,
}

/// Slots sealed per slice at the day's close (2,048 x TF_COUNT bars at most,
/// far under the seal queue), the queue drained between slices.
const CLOSE_SEAL_SLICE_SLOTS: usize = 2_048;

/// A WAL receipt (epoch nanoseconds) as IST fold seconds, or `None` when it
/// is negative or past `u32`. O(1).
fn receipt_fold_secs(receipt_nanos: i64) -> Option<u32> {
    if receipt_nanos <= 0 {
        return None;
    }
    let secs = receipt_nanos
        .div_euclid(1_000_000_000)
        .checked_add(19_800)?;
    u32::try_from(secs).ok().filter(|s| *s > 0)
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
        // LIVE fold semantics, not WAL-replay semantics (2026-11-21 review,
        // critical): the repaired day's partitions are dropped, so there is no
        // stored bar to protect, and replay mode would suppress every
        // instrument's first bucket in every timeframe (a new slot is born
        // partial) and skip the 09:15 first-session baseline. The live process
        // that captured the day booted before the open, so its first bars were
        // written; a restart inside the day is handled at the gap below, as a
        // live mid-session boot would handle it.
        self.aggregator.set_replay_mode(false);
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
                    // A restart (or an unread stretch) inside the day: as a live
                    // process booted mid-session, nothing before this receipt
                    // was captured by the frames that follow, so a bucket that
                    // started before it is withheld, never written short.
                    if let Some(capture) = receipt_fold_secs(*receipt) {
                        self.aggregator.set_live_capture_start(capture);
                    }
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

    /// `(ticks rows, tick_quotes rows, feed_aux_packets rows)` this ingest's
    /// writer appended. O(1).
    #[must_use]
    pub(crate) const fn past_day_row_counts(&self) -> (u64, u64, u64) {
        (
            self.writer.tick_rows_appended(),
            self.writer.quote_rows_appended(),
            self.writer.aux_rows_appended(),
        )
    }

    /// Ends the past-day refold: flushes the rows, seals every bucket up to
    /// `cutoff_ist_secs` (past the day's last bucket), runs the day-boundary
    /// seal, then the replay hand-over, all routed to the seal writer the
    /// same way the live catch-up seal routes them. O(slots × `TF_COUNT`).
    pub(crate) fn finish_past_day_refold(
        &mut self,
        cutoff_ist_secs: u32,
        mut pace: impl FnMut(),
    ) -> PastDayClose {
        blocking_flush(|| self.flush_ticks());
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let mut tally = SealTally::default();
        // In slices of `CLOSE_SEAL_SLICE_SLOTS`, the seal queue drained between
        // them (2026-11-21 review): every open bucket of every instrument seals
        // here at once, up to slots x TF_COUNT bars, which one burst could push
        // past the seal queue into the spill.
        let mut catch_up = 0_usize;
        let mut from = 0_usize;
        loop {
            pace();
            let (emitted, next) = self.aggregator.catch_up_seal_slots(
                cutoff_ist_secs,
                from,
                CLOSE_SEAL_SLICE_SLOTS,
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
            catch_up = catch_up.saturating_add(emitted);
            if next <= from {
                break;
            }
            from = next;
        }
        pace();
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
        assert_eq!(ingest.past_day_row_counts(), (0, 0, 0));
        assert_eq!(ingest.flush_past_day_rows(), 0);
    }

    #[test]
    fn test_finish_past_day_refold_ends_replay_and_clears_the_backup() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = 1_795_113_000u32.saturating_add(19_800);
        ingest.begin_past_day_refold(day_start, day_start + 9 * 3_600);
        let close = ingest.finish_past_day_refold(day_start + 16 * 3_600 + 300, || {});
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

#[cfg(test)]
mod live_fold_tests {
    use super::*;

    const DAY_START_UTC: i64 = 1_795_113_000; // 2026-11-20 00:00 IST

    /// One Ticker packet for NIFTY (13, IDX_I) whose raw LTT is the UTC form
    /// Dhan sent from 20 Nov; the dispatcher adds 19,800 s back.
    fn nifty_at(ist_secs_of_day: i64, ltp: f32) -> (i64, bytes::Bytes) {
        let utc = DAY_START_UTC + ist_secs_of_day;
        let mut f = vec![0u8; 16];
        f[0] = 2;
        f[1..3].copy_from_slice(&16u16.to_le_bytes());
        f[3] = 0;
        f[4..8].copy_from_slice(&13u32.to_le_bytes());
        f[8..12].copy_from_slice(&ltp.to_le_bytes());
        f[12..16].copy_from_slice(&u32::try_from(utc).expect("u32").to_le_bytes());
        (utc * 1_000_000_000, bytes::Bytes::from(f))
    }

    fn seals(ingest: &LiveIngest) -> u64 {
        ingest.seals_emitted() + ingest.seals_dropped() + ingest.seals_rescued()
    }

    #[test]
    fn test_receipt_fold_secs_adds_the_ist_offset() {
        assert_eq!(
            receipt_fold_secs(DAY_START_UTC * 1_000_000_000),
            u32::try_from(DAY_START_UTC + 19_800).ok()
        );
        assert_eq!(receipt_fold_secs(-1), None);
    }

    /// Regression (2026-11-21 review, critical): the rebuild ran in WAL-replay
    /// mode, where a new slot's first bucket is partial and suppressed, so the
    /// 09:15 bar of every instrument was never written. In live fold mode the
    /// 09:15 one-second and one-minute bars seal when the 09:16 tick arrives.
    #[test]
    fn test_regression_first_bucket_of_the_day_is_written_not_suppressed() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = u32::try_from(DAY_START_UTC + 19_800).expect("u32");
        // The live process began capturing at 08:31, before the open.
        ingest.begin_past_day_refold(day_start, day_start + 8 * 3_600 + 31 * 60);
        let (r1, f1) = nifty_at(9 * 3_600 + 15 * 60 + 1, 24_000.0);
        let (r2, f2) = nifty_at(9 * 3_600 + 16 * 60 + 5, 24_010.0);
        let frames = vec![
            (1u64, r1, WalEndpoint::MainFeed, f1),
            (2u64, r2, WalEndpoint::MainFeed, f2),
        ];
        let mut out = WalRefoldOutcome::default();
        ingest.refold_past_day_segment(&frames, &[], &mut out);
        assert_eq!(out.refolded, 2, "both ticks fold: {out:?}");
        assert!(
            seals(&ingest) >= 2,
            "the 09:15 1s and 1m bars must seal, got {}",
            seals(&ingest)
        );
    }

    fn packet(code: u8, len: u16, segment: u8, id: u32, body: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; usize::from(len)];
        p[0] = code;
        p[1..3].copy_from_slice(&len.to_le_bytes());
        p[3] = segment;
        p[4..8].copy_from_slice(&id.to_le_bytes());
        let n = body.len().min(p.len().saturating_sub(8));
        p[8..8 + n].copy_from_slice(&body[..n]);
        p
    }

    /// Non-tick packets reach the aux path, an order-update frame is walked,
    /// a truncated tail is abandoned and counted, never folded.
    #[test]
    fn test_non_tick_packets_and_a_truncated_tail_are_counted() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = u32::try_from(DAY_START_UTC + 19_800).expect("u32");
        ingest.begin_past_day_refold(day_start, day_start + 8 * 3_600 + 31 * 60);
        let receipt = (DAY_START_UTC + 10 * 3_600) * 1_000_000_000;
        let mut frame = packet(5, 12, 2, 4_242, &9_000i32.to_le_bytes());
        let mut prev = 24_100.5f32.to_le_bytes().to_vec();
        prev.extend_from_slice(&0i32.to_le_bytes());
        frame.extend(packet(6, 16, 0, 13, &prev));
        let (_, tick) = nifty_at(10 * 3_600 + 5, 24_000.0);
        frame.extend_from_slice(&tick);
        // A Ticker header that promises 16 bytes with only 10 left.
        frame.extend_from_slice(&packet(2, 16, 0, 13, &[])[..10]);
        let mut out = WalRefoldOutcome::default();
        ingest.refold_past_day_segment(
            &[(
                1u64,
                receipt,
                WalEndpoint::MainFeed,
                bytes::Bytes::from(frame),
            )],
            &[],
            &mut out,
        );
        assert_eq!(out.non_tick, 2, "{out:?}");
        assert_eq!(out.refolded, 1, "{out:?}");
        assert_eq!(out.unparseable, 1, "{out:?}");
        assert_eq!(out.abandoned_bytes, 10, "{out:?}");
        // An order-update frame goes through the same walk as a main-feed one;
        // an unknown code with no length to skip abandons the rest.
        let mut out = WalRefoldOutcome::default();
        let unknown = vec![99u8, 0, 0, 0, 1, 0, 0, 0];
        ingest.refold_past_day_segment(
            &[(
                2u64,
                receipt,
                WalEndpoint::OrderUpdate,
                bytes::Bytes::from(unknown),
            )],
            &[],
            &mut out,
        );
        assert_eq!(out.refolded, 0, "{out:?}");
        assert!(
            out.unparseable + out.unknown_skipped + out.undecodable >= 1,
            "{out:?}"
        );
        let rows = ingest.flush_past_day_rows();
        assert!(rows <= 4, "rows handed over: {rows}");
    }

    /// A restart gap inside the day: the bucket open across it is withheld
    /// (as a live mid-session boot withholds it), never written short.
    #[test]
    fn test_a_restart_gap_withholds_the_bucket_it_cuts() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let day_start = u32::try_from(DAY_START_UTC + 19_800).expect("u32");
        ingest.begin_past_day_refold(day_start, day_start + 8 * 3_600 + 31 * 60);
        let (r1, f1) = nifty_at(10 * 3_600 + 1, 24_000.0);
        // Downtime, then the restarted process's first frame mid-minute.
        let (r2, f2) = nifty_at(10 * 3_600 + 5 * 60 + 30, 24_050.0);
        let (r3, f3) = nifty_at(10 * 3_600 + 6 * 60 + 2, 24_060.0);
        let (r4, f4) = nifty_at(10 * 3_600 + 7 * 60 + 2, 24_070.0);
        let mut out = WalRefoldOutcome::default();
        ingest.refold_past_day_segment(&[(1u64, r1, WalEndpoint::MainFeed, f1)], &[], &mut out);
        let before = seals(&ingest);
        ingest.refold_past_day_segment(
            &[
                (2u64, r2, WalEndpoint::MainFeed, f2),
                (3u64, r3, WalEndpoint::MainFeed, f3),
                (4u64, r4, WalEndpoint::MainFeed, f4),
            ],
            &[0],
            &mut out,
        );
        assert_eq!(out.gaps_marked, 1);
        // The 10:06 minute (fully after the restart) seals at 10:07; the
        // 10:05 minute, which started before the restart, does not count.
        assert!(
            seals(&ingest) > before,
            "bars after the restart are written"
        );
    }
}
