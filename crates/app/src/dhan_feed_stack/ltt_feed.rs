//! The live lane's side of the Dhan trade-time correction (scope lock
//! 2026-11-20, "DHAN TRADE TIME SENT AS UTC"; the table, record, detector and
//! controller live in `tickvault_storage::ltt_correction`).
//!
//! Three seams, each O(1) per call and allocation-free:
//! - [`LiveIngest::ltt_on_live_frame`]: once per live WAL-stamped main-feed
//!   frame, BEFORE its packets are dispatched (a change the record thread has
//!   made durable takes effect here, past every receipt already dispatched);
//! - [`dispatch_main_feed_packet`]: the drain's dispatch, which never asks for
//!   a correction on a frame whose receipt is not a WAL stamp;
//! - [`LiveIngest::ltt_observe`]: one vote per fresh index tick or NSE stock
//!   spot print, on Dhan's RAW trade time, live frames only (never replay).
//!
//! [`replay_held`] stops a WAL replay before it folds a frame whose IST day
//! has a correction change that was started and never confirmed: the live run
//! may or may not have applied it, so no replay can know (the frames stay in
//! the log; coded `DHAN-LTT-02`).

use tickvault_common::config::LttUtcAutocorrect;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::parser::dispatcher::{
    dispatch_frame_uncounted, dispatch_frame_uncounted_unstamped, raw_last_trade_time,
};
use tickvault_core::parser::types::{ParseError, ParsedFrame};
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::ltt_correction::{
    LTT_RECORD_DIR, LttController, LttTable, RecordWriterHandle, SEGMENT_IDX_I, SEGMENT_NSE_EQ,
    any_receipt_day_held, install_table, spawn_record_writer,
};
use tracing::error;

use super::LiveIngest;

/// The inert controller every `LiveIngest` starts with (tests, and a lane
/// built without the production builder): it never votes, records or arms.
pub(super) fn inert_controller() -> LttController {
    LttController::with_table(
        LttUtcAutocorrect::Off,
        None,
        LttTable::empty(),
        install_table,
    )
}

/// Hands the Telegram service to the correction's record thread, which sends
/// the once-a-day notice when the correction arms (owner card "Yes, one a day
/// (Recommended)", noise lock §2.24): instant, never an SMS page.
pub fn install_ltt_notice(
    notifier: &std::sync::Arc<tickvault_core::notification::service::NotificationService>,
) {
    let notifier = std::sync::Arc::clone(notifier);
    // `notify` spawns a tokio task; the record thread is a plain OS thread, so
    // it enters the runtime captured here first (review fix 2026-11-21: without
    // it the spawn panics and `panic = "abort"` ends the process).
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "ltt_notice_no_runtime",
            "the trade-time correction notice could not be installed (no runtime)"
        );
        return;
    };
    tickvault_storage::ltt_correction::install_notice(std::sync::Arc::new(move |message| {
        let _entered = runtime.enter();
        notifier.notify(
            tickvault_core::notification::events::NotificationEvent::CustomStatusUrgent { message },
        );
    }));
}

/// The record writer thread for `act` mode (shadow and off write nothing).
/// A spawn failure is one coded error and no writer: every change the
/// detector proposes is then refused (counted), never applied unrecorded.
pub(super) fn production_writer(mode: LttUtcAutocorrect) -> Option<RecordWriterHandle> {
    if mode != LttUtcAutocorrect::Act {
        return None;
    }
    match spawn_record_writer(std::path::PathBuf::from(LTT_RECORD_DIR), None) {
        Ok(writer) => Some(writer),
        Err(err) => {
            error!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "ltt_record_writer_spawn_failed",
                %err,
                "The Dhan trade-time correction record thread could not start, so the \
                 correction cannot arm today (raw trade times are used)."
            );
            None
        }
    }
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
    /// Arms the production trade-time correction: the controller over the
    /// boot-loaded table, writing its record through `writer`.
    #[must_use]
    pub fn with_ltt_correction(
        mut self,
        mode: LttUtcAutocorrect,
        writer: Option<RecordWriterHandle>,
    ) -> Self {
        self.ltt = LttController::new(mode, writer);
        self
    }

    /// The trade-time controller (tests and the drain's counters).
    #[must_use]
    pub fn ltt_controller(&self) -> &LttController {
        &self.ltt
    }

    /// Once per main-feed frame, before its packets are dispatched. Only a
    /// live frame with a WAL receipt stamp counts. O(1).
    #[inline]
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

    #[test]
    fn test_inert_controller_never_votes_or_arms() {
        let mut ctl = inert_controller();
        let receipt = 1_795_152_600 * 1_000_000_000;
        ctl.on_frame(receipt);
        for i in 0..200u64 {
            ctl.observe(SEGMENT_IDX_I, i % 5, 1_795_152_600, receipt);
        }
        assert_eq!(ctl.counts().votes, 0);
        assert_eq!(ctl.counts().armed, 0);
    }

    #[test]
    fn test_replay_held_is_false_with_nothing_held() {
        let frames: Vec<(u64, i64, (), ())> = vec![(1, 1_795_152_600 * 1_000_000_000, (), ())];
        assert!(!replay_held(&frames, "test"));
        assert!(!replay_held::<(), ()>(&[], "test"));
    }

    #[test]
    fn test_dispatch_main_feed_packet_parses_both_ways_with_nothing_armed() {
        let mut ticker = vec![0u8; 16];
        ticker[0] = 2;
        ticker[12..16].copy_from_slice(&1_795_152_600u32.to_le_bytes());
        let receipt = 1_795_152_600 * 1_000_000_000;
        let stamped = format!("{:?}", dispatch_main_feed_packet(&ticker, receipt, true));
        let unstamped = format!("{:?}", dispatch_main_feed_packet(&ticker, receipt, false));
        assert_eq!(stamped, unstamped);
    }
}
