//! The live lane's side of the Dhan trade-time correction (the rule and the
//! reversion guard live in `tickvault_storage::ltt_correction`).
//!
//! ⚠ CHANGED 2026-11-21 (scope lock "DHAN LIVE LTT IS UTC EPOCH FROM RECEIPT
//! 2026-11-20 00:00 IST"; Dhan ticket #6684672): the correction is a pure
//! function of `(raw, receipt)` applied in the core dispatcher, the same live
//! and in replay. The lane starts no record writer and sends no daily notice.
//!
//! Three seams, each O(1) per call and allocation-free:
//! - [`LiveIngest::ltt_on_live_frame`]: once per live WAL-stamped main-feed
//!   frame, BEFORE its packets are dispatched (rolls the guard's day and
//!   publishes the refusal counters once a minute);
//! - [`dispatch_main_feed_packet`]: the drain's dispatch, which never asks for
//!   a correction on a frame whose receipt is not a WAL stamp;
//! - [`LiveIngest::ltt_observe`]: one vote per fresh index tick or NSE stock
//!   spot print, on Dhan's RAW trade time, live frames only (never replay), for
//!   the reversion guard (pages DHAN-FEED-TIME-01 once per segment per day;
//!   in `act` the drain's feed-time scan then holds that day, Quote 63).
//!
//! [`replay_held`] is DORMANT since 2026-11-21: no day is ever held (the boot
//! step reads no record), so it always returns false. Deleted in the
//! follow-up PR.

use tickvault_common::config::LttUtcAutocorrect;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::parser::dispatcher::{
    dispatch_frame_uncounted, dispatch_frame_uncounted_unstamped, raw_last_trade_time,
};
use tickvault_core::parser::types::{ParseError, ParsedFrame};
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::ltt_correction::{
    LttController, RecordWriterHandle, SEGMENT_IDX_I, SEGMENT_NSE_EQ, any_receipt_day_held,
    publish_ltt_counters,
};
use tracing::error;

use super::LiveIngest;

/// The inert guard every `LiveIngest` starts with (tests, and a lane built
/// without the production builder): it never votes, reports or publishes.
pub(super) fn inert_controller() -> LttController {
    LttController::inert()
}

/// The record writer. ⚠ RETIRED 2026-11-21: always `None` (the correction no
/// longer arms from a record, so nothing writes one). Kept so the lane's
/// builder call is unchanged; deleted in the follow-up PR.
// TEST-EXEMPT: covered by test_production_writer_is_retired
pub(super) fn production_writer(_mode: LttUtcAutocorrect) -> Option<RecordWriterHandle> {
    None
}

/// The drain's packet dispatch: the correction is asked for only when the
/// frame's receipt is a WAL stamp (scope lock 2026-11-20: never on a frame
/// without one). O(1), no allocation.
#[inline]
pub(super) fn dispatch_main_feed_packet(
    packet: &[u8],
    received_at_nanos: i64,
    receipt_is_wal_stamp: bool,
) -> Result<ParsedFrame, ParseError> {
    if receipt_is_wal_stamp {
        dispatch_frame_uncounted(packet, received_at_nanos)
    } else {
        dispatch_frame_uncounted_unstamped(packet, received_at_nanos)
    }
}

impl LiveIngest {
    /// Installs the production reversion guard in `mode` (scope lock
    /// 2026-11-21). `_writer` is ignored (the record writer is retired; the
    /// builder always passes `None`). In `act` a reversion page also holds its
    /// day (the drain's feed-time scan hands it to the latch thread).
    #[must_use]
    // TEST-EXEMPT: covered by test_regression_ist_epoch_index_prints_page_the_reversion_guard_once (app/tests)
    pub fn with_ltt_correction(
        mut self,
        mode: LttUtcAutocorrect,
        _writer: Option<RecordWriterHandle>,
    ) -> Self {
        self.ltt = LttController::new(mode);
        // Registers the refusal and unstamped counters at their current
        // values (0 at boot) so the series exist before the first live frame.
        publish_ltt_counters();
        self
    }

    /// The trade-time controller (tests and the drain's counters).
    #[must_use]
    // TEST-EXEMPT: covered by test_frames_without_a_wal_stamp_or_from_replay_never_vote (app/tests)
    pub fn ltt_controller(&self) -> &LttController {
        &self.ltt
    }

    /// Once per main-feed frame, before its packets are dispatched. Only a
    /// live frame with a WAL receipt stamp counts. O(1).
    #[inline]
    // TEST-EXEMPT: covered by test_regression_2026_11_20_utc_index_prints_never_page_and_are_corrected and test_frames_without_a_wal_stamp_or_from_replay_never_vote (app/tests)
    pub(super) fn ltt_on_live_frame(&mut self, frame: &CapturedFrame) {
        if frame.connection_index == u8::MAX {
            return;
        }
        self.feed_time.note_frame();
        if self.frame_receipt_is_wal_stamp {
            self.ltt.on_frame(frame.received_at_nanos);
        }
    }

    /// One vote for a live index tick or NSE stock spot print, on Dhan's raw
    /// trade time (read from the packet bytes, before the correction). O(1).
    #[inline]
    // TEST-EXEMPT: covered by test_frames_without_a_wal_stamp_or_from_replay_never_vote (app/tests)
    pub(super) fn ltt_observe(&mut self, packet: &[u8], tick: &ParsedTick, frame: &CapturedFrame) {
        let segment = tick.exchange_segment_code;
        if frame.connection_index == u8::MAX
            || !self.frame_receipt_is_wal_stamp
            || !(segment == SEGMENT_IDX_I || segment == SEGMENT_NSE_EQ)
        {
            return;
        }
        if segment == SEGMENT_IDX_I {
            // §2.24 cause A judges the time AFTER the correction.
            self.feed_time
                .note_index_tick(tick.exchange_timestamp, frame.received_at_nanos);
        }
        if let Some(raw) = raw_last_trade_time(packet) {
            self.ltt
                .observe(segment, tick.security_id, raw, frame.received_at_nanos);
        }
    }
}

/// True (and one coded `DHAN-LTT-02` line) when any frame's IST day is held
/// from replay. O(1) when no day is held; otherwise one slot read per frame,
/// once per replay pass, cold.
pub(super) fn replay_held<E, B>(frames: &[(u64, i64, E, B)], pass: &'static str) -> bool {
    let held = any_receipt_day_held(frames.iter().map(|f| f.1));
    if held {
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "ltt_replay_held",
            pass,
            frames = frames.len(),
            "A WAL replay reached a day whose Dhan trade-time correction was started and never \
             confirmed. The live run may or may not have corrected those prices, so the replay \
             stops here and the frames stay in the log (nothing is lost). The day needs the \
             manual repair."
        );
    }
    held
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-20 11:00:00 IST as UTC epoch seconds.
    const T: i64 = 1_795_152_600;

    #[test]
    fn test_inert_controller_never_votes_or_reports() {
        let mut ctl = inert_controller();
        let receipt = T * 1_000_000_000;
        ctl.on_frame(receipt);
        for i in 0..200u64 {
            ctl.observe(SEGMENT_IDX_I, i % 5, (T + 19_800) as u32, receipt);
        }
        assert_eq!(ctl.counts().votes, 0);
        assert_eq!(ctl.counts().reversion_reports, 0);
    }

    #[test]
    fn test_regression_2026_11_21_building_the_guard_registers_its_counters() {
        // Regression: 2026-11-21 review — the refusal and unstamped counters
        // first appeared at the first live frame, so a down lane or a boot
        // replay showed no series. Building the guard now registers them.
        let src = include_str!("ltt_feed.rs");
        let start = src.find("fn with_ltt_correction(").expect("builder");
        let body = &src[start..start + src[start..].find("\n    }\n").expect("end")];
        let built = body.find("LttController::new(mode);").expect("guard built");
        let published = body.find("publish_ltt_counters();").expect("registered");
        assert!(built < published, "{body}");
    }

    #[test]
    fn test_production_writer_is_retired() {
        for mode in [
            LttUtcAutocorrect::Off,
            LttUtcAutocorrect::Shadow,
            LttUtcAutocorrect::Act,
        ] {
            assert!(production_writer(mode).is_none());
        }
    }

    #[test]
    fn test_replay_held_is_false_with_nothing_held() {
        let frames: Vec<(u64, i64, (), ())> = vec![(1, T * 1_000_000_000, (), ())];
        assert!(!replay_held(&frames, "test"));
        assert!(!replay_held::<(), ()>(&[], "test"));
    }

    fn ticker(raw: u32) -> Vec<u8> {
        let mut ticker = vec![0u8; 16];
        ticker[0] = 2;
        ticker[12..16].copy_from_slice(&raw.to_le_bytes());
        ticker
    }

    fn ltt_of(frame: Result<ParsedFrame, ParseError>) -> u32 {
        match frame {
            Ok(ParsedFrame::Tick(t)) => t.exchange_timestamp,
            other => panic!("not a tick: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_main_feed_packet_corrects_only_a_wal_stamped_frame_from_the_cutover() {
        let raw = T as u32;
        let receipt = T * 1_000_000_000;
        assert_eq!(
            ltt_of(dispatch_main_feed_packet(&ticker(raw), receipt, true)),
            raw + 19_800
        );
        assert_eq!(
            ltt_of(dispatch_main_feed_packet(&ticker(raw), receipt, false)),
            raw
        );
        // Before 2026-11-20 00:00 IST both paths are identical.
        let old = receipt - 2 * 86_400 * 1_000_000_000;
        let stamped = format!("{:?}", dispatch_main_feed_packet(&ticker(raw), old, true));
        let unstamped = format!("{:?}", dispatch_main_feed_packet(&ticker(raw), old, false));
        assert_eq!(stamped, unstamped);
    }
}
