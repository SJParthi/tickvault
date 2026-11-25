//! The drain's per-frame counter tally (plan ITEM DT, sub-µs drain,
//! 2026-10-30; speed fix 4, 2026-11-17). Moved out of `dhan_feed_stack.rs`
//! unchanged on 2026-11-17 to keep that file under its size pin
//! (`source_size_ratchet_guard`). O(1) per packet, no allocation; every
//! counter is added once per frame when the tally drops.

use tickvault_core::parser::dispatcher::count_packets_by_response_code;

use super::DrainCounters;

/// Per-frame counter sums for [`drain_main_feed_frame`](super::drain_main_feed_frame)
/// (plan ITEM DT, sub-µs drain, 2026-10-30), added to the shared counters once, when the
/// tally drops at the end of the walk (every return path).
///
/// `metrics` counters are atomic read-modify-writes once a recorder is
/// installed. The drain paid two per tick packet: the response-code count
/// inside `dispatch_frame` and the `folded` outcome. Here the code count is
/// kept as one run of equal codes (a frame is almost always one code
/// throughout), flushed when the code changes, so a frame costs one increment
/// per run and one for `folded`, whatever its packet count. The totals equal
/// the per-packet increments exactly; only when they land moves, by at most
/// one frame.
///
/// O(1) per packet: one compare and one add. No allocation.
///
/// ⚠ CHANGED 2026-11-17 (speed fix 4): four more per-tick increments moved
/// here, each a success or exclusion count, never a refusal or loss count:
/// `tv_dhan_feed_ingest_ticks_total` (one per row written, which is exactly
/// one per `folded`: [`LiveIngest::ingest_tick_at`] returns `Folded` or
/// `WrittenOutOfSession` only after the row is appended, and those are the
/// only two arms that add to `folded`), and the three lag exclusions
/// `ltt_not_advanced`, `not_new_trade` and `not_a_trade` of
/// `tv_dhan_ws_lag_excluded_total`. The clamp and garbage-stamp exclusions
/// stay per tick in `record_ws_lag`: they are the clock-fault signal.
/// Totals equal the per-tick increments at the end of every frame, which is
/// before any other drain arm (the 1,024-frame publish, the flush timer, the
/// shutdown arm) runs.
pub(super) struct FrameTally<'c> {
    counters: &'c DrainCounters,
    /// Packets folded or written this frame (the `folded` outcome); also the
    /// rows written, added to `ingest_ticks` at the end of the frame.
    pub(super) folded: u64,
    /// A REPEATED quote (same trade time, price and cumulative volume as the
    /// previous packet). Its stamp is the time of an earlier trade, so its
    /// "lag" is how long the instrument has been quiet, not transit time.
    /// Counted under `ltt_not_advanced`, never recorded.
    lag_repeat: u64,
    /// A folded packet that carried no NEW trade (plan PR AL-1, 2026-11-06):
    /// the first packet after a subscribe, a stale regression, a backup copy
    /// accepted late. Counted under `not_new_trade`, never recorded.
    lag_not_new_trade: u64,
    /// A tick the fold did not accept as a trade (out of session, an earlier
    /// trading day, a refused price or time, a failed write). Counted under
    /// `not_a_trade`, never recorded (2026-10-05).
    lag_not_a_trade: u64,
    /// The current run of one response code.
    run: CodeRun,
}

impl<'c> FrameTally<'c> {
    pub(super) const fn new(counters: &'c DrainCounters) -> Self {
        Self {
            counters,
            folded: 0,
            lag_repeat: 0,
            lag_not_new_trade: 0,
            lag_not_a_trade: 0,
            run: CodeRun::new(),
        }
    }

    /// Counts one dispatched packet under its response code.
    #[inline]
    pub(super) fn count_packet(&mut self, code: u8) {
        if let Some((done, packets)) = self.run.push(code) {
            count_packets_by_response_code(done, packets);
        }
    }

    /// A repeated quote, excluded from the lag measurement. One add.
    #[inline]
    pub(super) fn exclude_repeat(&mut self) {
        self.lag_repeat = self.lag_repeat.saturating_add(1);
    }

    /// A folded packet with no new trade, excluded from the lag measurement.
    /// One add.
    #[inline]
    pub(super) fn exclude_not_new_trade(&mut self) {
        self.lag_not_new_trade = self.lag_not_new_trade.saturating_add(1);
    }

    /// A tick the fold did not accept as a trade, excluded from the lag
    /// measurement. One add.
    #[inline]
    pub(super) fn exclude_not_a_trade(&mut self) {
        self.lag_not_a_trade = self.lag_not_a_trade.saturating_add(1);
    }
}

impl Drop for FrameTally<'_> {
    fn drop(&mut self) {
        if let Some((code, packets)) = self.run.take() {
            count_packets_by_response_code(code, packets);
        }
        if self.folded > 0 {
            self.counters.folded.increment(self.folded);
            self.counters.ingest_ticks.increment(self.folded);
        }
        if self.lag_repeat > 0 {
            self.counters
                .lag_excluded_ltt_not_advanced
                .increment(self.lag_repeat);
        }
        if self.lag_not_new_trade > 0 {
            self.counters
                .lag_excluded_not_new_trade
                .increment(self.lag_not_new_trade);
        }
        if self.lag_not_a_trade > 0 {
            self.counters
                .lag_excluded_not_a_trade
                .increment(self.lag_not_a_trade);
        }
    }
}

/// One run of equal response codes, for [`FrameTally`]. Pure, so the "every
/// packet counted once under its own code" rule is property-tested on its
/// own (`frame_tally_tests`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CodeRun {
    /// Response code of the run; meaningful only while `len > 0`.
    code: u8,
    /// Packets in the run.
    len: u64,
}

impl CodeRun {
    pub(super) const fn new() -> Self {
        Self { code: 0, len: 0 }
    }

    /// Adds one packet of `code`. Returns the run it ended, `(code, packets)`,
    /// when `code` differs from the current run's. O(1).
    #[inline]
    pub(super) fn push(&mut self, code: u8) -> Option<(u8, u64)> {
        let ended = (self.len > 0 && code != self.code).then_some((self.code, self.len));
        if ended.is_some() {
            self.len = 0;
        }
        self.code = code;
        self.len = self.len.saturating_add(1);
        ended
    }

    /// Ends the current run, returning it when it holds any packet. O(1).
    #[inline]
    pub(super) fn take(&mut self) -> Option<(u8, u64)> {
        let run = (self.len > 0).then_some((self.code, self.len));
        self.len = 0;
        run
    }
}
