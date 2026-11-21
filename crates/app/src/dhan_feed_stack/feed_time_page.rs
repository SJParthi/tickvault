//! `DHAN-FEED-TIME-01`: the live feed's trade times still look wrong after the
//! correction (noise lock §2.24, owner cards "Yes, page me (Recommended)",
//! "After correction (Recommended)", "Yes, hold that day (Recommended)",
//! 2026-11-20).
//!
//! On 20 Nov 2026 Dhan sent trade times as UTC and nothing paged until the
//! 15:50 cross-check. Two causes, each at most once per IST day, judged only
//! inside the session (09:15–15:30 IST, or the published Muhurat window) and
//! only from the live drain (it never runs during a WAL replay):
//!
//! - **A, prices outside the session:** in two 30 s scans in a row, at least
//!   [`CAUSE_A_MIN_TICKS`] live index ticks, and at least half of them carry a
//!   trade time (AFTER the correction) outside the session or more than
//!   [`CAUSE_A_FAR_SECS`] from receipt.
//! - **B, no candles:** [`CAUSE_B_SCANS`] scans in a row (10 minutes) with
//!   main-feed frames arriving in each and no candle sealed.
//!
//! The drain only counts (plain `u64` adds per index tick and per frame) and,
//! once a cause fires, emits one coded `error!` and hands the day to a writer
//! thread: the once-a-day latch and the day's hold marker are written THERE,
//! never on the drain (§2.24 REJECT "persisting the latch on the drain"). A
//! restart restores the latch, so a cause never pages twice in a day.
//!
//! ⚠ ADDED 2026-11-21 (noise lock §2.24-a, daily-universe Quote 63): the
//! trade-time reversion guard pages through its own `error!`; each 30 s scan
//! reads the guard's page count and hands that day to the same writer thread,
//! which writes the day's `.hold` (no latch marker; the guard's latch is in
//! RAM). See [`FeedTimePage::hold_after_reversion_page`].

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use chrono::NaiveDate;
use tickvault_common::constants::{
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::ltt_correction::{ist_day_date, receipt_ist_day, receipt_ist_secs};
use tracing::error;

use super::LiveIngest;

/// Cause A: index ticks a scan needs before it can judge.
pub(super) const CAUSE_A_MIN_TICKS: u64 = 20;
/// Cause A: scans in a row.
pub(super) const CAUSE_A_SCANS: u32 = 2;
/// Cause A: a trade time this far from receipt is wrong.
pub(super) const CAUSE_A_FAR_SECS: i64 = 300;
/// Cause B: scans in a row with frames and no candle (20 × 30 s = 10 min).
pub(super) const CAUSE_B_SCANS: u32 = 20;
/// A Muhurat day's trade-time window: the published session, 15 min before
/// and 10 min after (as 09:15-15:30 widens to 09:00-15:40).
/// One source with the reversion guard's window (review fix 2026-11-21).
const MUHURAT_WINDOW_LEAD_SECS: u32 = tickvault_storage::ltt_correction::MUHURAT_GUARD_LEAD_SECS;
const MUHURAT_WINDOW_TAIL_SECS: u32 = tickvault_storage::ltt_correction::MUHURAT_GUARD_TAIL_SECS;
/// Daily-marker task names (one per cause), kept 7 days.
const TASK_CAUSE_A: &str = "feed-time-page-a";
const TASK_CAUSE_B: &str = "feed-time-page-b";
/// Hand-offs the writer thread can hold (at most three a day: causes A and
/// B, and one reversion hold).
const PERSIST_DEPTH: usize = 4;

/// Which cause fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cause {
    /// Trade times outside the session after the correction.
    PricesOutsideSession,
    /// Frames arriving and no candle sealed for 10 minutes.
    NoCandles,
    /// The trade-time reversion guard paged (noise lock §2.24-a): its own
    /// `error!` is the page; this cause only carries the day to the writer
    /// thread for the hold (daily-universe Quote 63). No latch marker.
    FormatReverted,
}

impl Cause {
    const fn index(self) -> usize {
        match self {
            Self::PricesOutsideSession => 0,
            Self::NoCandles => 1,
            Self::FormatReverted => 2,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::PricesOutsideSession => "prices_outside_session",
            Self::NoCandles => "no_candles",
            Self::FormatReverted => "dhan_time_format_reverted",
        }
    }

    /// The daily latch marker (causes A and B; the reversion guard keeps its
    /// own latch in RAM, so it writes none).
    const fn task(self) -> Option<&'static str> {
        match self {
            Self::PricesOutsideSession => Some(TASK_CAUSE_A),
            Self::NoCandles => Some(TASK_CAUSE_B),
            Self::FormatReverted => None,
        }
    }
}

/// The live drain's per-tick and per-frame counts (plain adds, single owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FeedTimeTally {
    index_ticks: u64,
    index_bad: u64,
    main_frames: u64,
    /// The session window a trade time must fall in (secs of day, IST).
    window: (u32, u32),
}

impl Default for FeedTimeTally {
    fn default() -> Self {
        Self {
            index_ticks: 0,
            index_bad: 0,
            main_frames: 0,
            window: (
                TICK_PERSIST_START_SECS_OF_DAY_IST,
                TICK_PERSIST_END_SECS_OF_DAY_IST,
            ),
        }
    }
}

impl FeedTimeTally {
    /// One live main-feed frame. O(1).
    #[inline]
    pub(super) fn note_frame(&mut self) {
        self.main_frames = self.main_frames.wrapping_add(1);
    }

    /// One live index tick with its trade time AFTER the correction. O(1).
    #[inline]
    // TEST-EXEMPT: covered by test_tally_judges_corrected_times_against_the_window_and_receipt
    pub(super) fn note_index_tick(&mut self, ltt_ist_secs: u32, receipt_nanos: i64) {
        self.index_ticks = self.index_ticks.wrapping_add(1);
        let t = i64::from(ltt_ist_secs);
        let sod = t.rem_euclid(86_400) as u32;
        let outside = !(self.window.0..self.window.1).contains(&sod);
        let far = (t - receipt_ist_secs(receipt_nanos)).abs() > CAUSE_A_FAR_SECS;
        if outside || far {
            self.index_bad = self.index_bad.wrapping_add(1);
        }
    }
}

/// The per-day episode: pure, so every rule is testable without a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FeedTimeEpisode {
    day: i64,
    fired: [bool; 2],
    a_streak: u32,
    b_streak: u32,
    last: (u64, u64, u64, u64),
}

impl FeedTimeEpisode {
    /// A new episode for `day`, with the latches a restart restored.
    pub(super) const fn new(day: i64, fired: [bool; 2]) -> Self {
        Self {
            day,
            fired,
            a_streak: 0,
            b_streak: 0,
            last: (0, 0, 0, 0),
        }
    }

    /// One 30 s scan. `totals` are cumulative (index ticks, bad index ticks,
    /// main-feed frames, sealed candles). Returns the causes that fire now.
    pub(super) fn scan(
        &mut self,
        day: i64,
        in_gate: bool,
        restored: [bool; 2],
        totals: (u64, u64, u64, u64),
    ) -> [Option<Cause>; 2] {
        let (ticks, bad, frames, seals) = totals;
        let d_ticks = ticks.wrapping_sub(self.last.0);
        let d_bad = bad.wrapping_sub(self.last.1);
        let d_frames = frames.wrapping_sub(self.last.2);
        let d_seals = seals.wrapping_sub(self.last.3);
        self.last = totals;
        if day != self.day {
            *self = Self::new(day, restored);
            self.last = totals;
            return [None, None];
        }
        if !in_gate {
            self.a_streak = 0;
            self.b_streak = 0;
            return [None, None];
        }
        let a_bad = d_ticks >= CAUSE_A_MIN_TICKS && d_bad.saturating_mul(2) >= d_ticks;
        self.a_streak = if a_bad {
            self.a_streak.saturating_add(1)
        } else {
            0
        };
        let b_bad = d_frames > 0 && d_seals == 0;
        self.b_streak = if b_bad {
            self.b_streak.saturating_add(1)
        } else {
            0
        };
        let mut out = [None, None];
        if self.a_streak >= CAUSE_A_SCANS && !self.fired[0] {
            self.fired[0] = true;
            out[0] = Some(Cause::PricesOutsideSession);
        }
        if self.b_streak >= CAUSE_B_SCANS && !self.fired[1] {
            self.fired[1] = true;
            out[1] = Some(Cause::NoCandles);
        }
        out
    }
}

/// The drain-side page: the episode plus the hand-off to the writer thread.
#[derive(Debug)]
pub(super) struct FeedTimePage {
    episode: FeedTimeEpisode,
    restored_day: i64,
    restored: [bool; 2],
    persist: Option<SyncSender<(NaiveDate, Cause)>>,
    /// Reversion pages already seen (the guard's count).
    reversion_pages_seen: u32,
    /// The last day a reversion hold was handed off (once per day).
    reversion_held_day: i64,
    /// A reversion page's day not yet handed off (writer missing or full):
    /// every later scan tries again (review fix 2026-11-21).
    reversion_pending_day: Option<i64>,
    /// `(IST day, cause bits)` of hand-off failures already logged, so a
    /// retry every 30 s logs once per cause per day.
    handoff_failures_logged: (i64, u8),
}

fn restore_latch(day: i64) -> [bool; 2] {
    let Some(date) = ist_day_date(day) else {
        return [false; 2];
    };
    [
        crate::daily_task_marker::daily_marker_exists(TASK_CAUSE_A, date),
        crate::daily_task_marker::daily_marker_exists(TASK_CAUSE_B, date),
    ]
}

fn run_persist(rx: &Receiver<(NaiveDate, Cause)>) {
    while let Ok((date, cause)) = rx.recv() {
        if let Some(task) = cause.task()
            && let Err(err) = crate::daily_task_marker::try_write_daily_marker(task, date)
        {
            error!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "feed_time_latch_write_failed",
                cause = cause.label(),
                %err,
                "Could not save today's feed-time page latch; a restart today may page again."
            );
        }
        let reason = format!("feed-time page {}", cause.label());
        if let Err(err) = tickvault_storage::held_days::write_day_marker(
            std::path::Path::new(tickvault_storage::held_days::HELD_DAYS_DIR),
            tickvault_storage::held_days::MarkerKind::Hold,
            date,
            tickvault_storage::held_days::today_ist(),
            &reason,
        ) {
            error!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "feed_time_hold_write_failed",
                %date,
                %err,
                "Could not hold today out of the archive after the feed-time page. Write \
                 data/state/held-days/<date>.hold by hand before the evening archive runs."
            );
        }
    }
}

impl FeedTimePage {
    /// Restores today's latches and starts the writer thread. Cold: once per
    /// drain, before its loop (two small file reads).
    pub(super) fn start() -> Self {
        let day = super::ist_day_number_now();
        let restored = restore_latch(day);
        let (tx, rx) = sync_channel(PERSIST_DEPTH);
        let persist = match std::thread::Builder::new()
            .name("tv-feed-time-latch".to_string())
            .spawn(move || run_persist(&rx))
        {
            Ok(_) => Some(tx),
            Err(err) => {
                // Review fix 2026-11-21: this was silent, and every later
                // hold hand-off then did nothing (noise lock §2.24-a).
                error!(
                    code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                    source = "feed_time_writer_missing",
                    %err,
                    "Could not start the feed-time page's writer; a feed-time page today                      cannot hold its day. Write data/state/held-days/<date>.hold by hand if                      one fires."
                );
                None
            }
        };
        Self::with_writer(day, restored, persist)
    }

    fn with_writer(
        day: i64,
        restored: [bool; 2],
        persist: Option<SyncSender<(NaiveDate, Cause)>>,
    ) -> Self {
        Self {
            episode: FeedTimeEpisode::new(day, restored),
            restored_day: day,
            restored,
            persist,
            reversion_pages_seen: 0,
            reversion_held_day: i64::MIN,
            reversion_pending_day: None,
            handoff_failures_logged: (i64::MIN, 0),
        }
    }

    /// One 30 s scan on the drain's silence arm. O(1); a day change reads the
    /// new day's two markers once (cold).
    pub(super) fn scan(&mut self, ingest: &mut LiveIngest) {
        let now_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let day = receipt_ist_day(now_nanos);
        if day != self.restored_day {
            self.restored_day = day;
            self.restored = restore_latch(day);
        }
        let sod = receipt_ist_secs(now_nanos).rem_euclid(86_400) as u64;
        let muhurat = ist_day_date(day).and_then(|date| {
            super::TRADING_CALENDAR
                .get()
                .and_then(|c| c.muhurat_session_secs(date))
        });
        let in_gate = match muhurat {
            Some((start, end)) => (u64::from(start)..u64::from(end)).contains(&sod),
            None => super::silence_page_allowed_today() && super::is_within_market_hours_ist(sod),
        };
        ingest.feed_time.window = if let Some((start, end)) = muhurat {
            // The PUBLISHED Muhurat session (review fix 2026-11-21: a fixed
            // 18:00-19:30 window misjudged an afternoon session), widened like
            // 09:15-15:30 is to 09:00-15:40.
            (
                start.saturating_sub(MUHURAT_WINDOW_LEAD_SECS),
                end.saturating_add(MUHURAT_WINDOW_TAIL_SECS),
            )
        } else {
            (
                TICK_PERSIST_START_SECS_OF_DAY_IST,
                TICK_PERSIST_END_SECS_OF_DAY_IST,
            )
        };
        let t = ingest.feed_time;
        let seals = ingest.seals_emitted.saturating_add(ingest.seals_rescued);
        let fired = self.episode.scan(
            day,
            in_gate,
            self.restored,
            (t.index_ticks, t.index_bad, t.main_frames, seals),
        );
        for cause in fired.into_iter().flatten() {
            self.fire(day, cause);
        }
        let ltt = ingest.ltt.counts();
        self.hold_after_reversion_page(ltt.reversion_pages, ltt.last_reversion_day);
    }

    /// Holds the day of a reversion page (daily-universe Quote 63; noise lock
    /// §2.24-a): when the guard's page count rose since the last scan, hands
    /// `day` to the writer thread once (it writes the `.hold`, off the drain).
    /// A hand-off that fails (writer missing, queue full) is logged as
    /// DHAN-LTT-02 and tried again at the next scan; the day is marked held
    /// only once it was handed off. Logs no page of its own: the guard's
    /// `error!` was the page. O(1), one `try_send`, no allocation. Returns
    /// true when it handed the day off.
    // TEST-EXEMPT: covered by test_regression_2026_11_21_a_reversion_page_holds_its_day_once
    pub(super) fn hold_after_reversion_page(&mut self, pages: u32, day: i64) -> bool {
        if pages != self.reversion_pages_seen {
            self.reversion_pages_seen = pages;
            if day != self.reversion_held_day {
                self.reversion_pending_day = Some(day);
            }
        }
        let Some(pending) = self.reversion_pending_day else {
            return false;
        };
        let Some(date) = ist_day_date(pending) else {
            self.reversion_pending_day = None;
            return false;
        };
        if !self.hand_off(pending, date, Cause::FormatReverted) {
            return false;
        }
        self.reversion_held_day = pending;
        self.reversion_pending_day = None;
        true
    }

    /// Pages cause A or B. The reversion guard pages through its own
    /// `error!` (§2.24-a REJECT: "a second DHAN-FEED-TIME-01 line from the
    /// hold hand-off"), so `FormatReverted` never reaches here.
    fn fire(&mut self, day: i64, cause: Cause) {
        debug_assert!(
            cause != Cause::FormatReverted,
            "fire takes causes A and B only"
        );
        let what = match cause {
            Cause::PricesOutsideSession => {
                "most index prices carry a time outside market hours (after our correction)"
            }
            Cause::NoCandles => "no candle has formed for 10 minutes while prices arrive",
            Cause::FormatReverted => return,
        };
        error!(
            code = ErrorCode::DhanFeedTime01TradeTimesWrong.code_str(),
            cause = cause.label(),
            "Dhan feed times look wrong during the session: {what}. Today is held out of the \
             archive and the export until it is repaired; raw data is kept."
        );
        if let Some(index) = self.restored.get_mut(cause.index()) {
            *index = true;
        }
        let Some(date) = ist_day_date(day) else {
            return;
        };
        self.hand_off(day, date, cause);
    }

    /// Hands `date` to the writer thread. Returns true when it was queued; a
    /// failure logs one DHAN-LTT-02 line per cause per day.
    fn hand_off(&mut self, day: i64, date: NaiveDate, cause: Cause) -> bool {
        let queued = match &self.persist {
            Some(tx) => tx.try_send((date, cause)).is_ok(),
            None => false,
        };
        if queued {
            return true;
        }
        let bit = 1u8 << cause.index();
        if self.handoff_failures_logged.0 != day {
            self.handoff_failures_logged = (day, 0);
        }
        if self.handoff_failures_logged.1 & bit != 0 {
            return false;
        }
        self.handoff_failures_logged.1 |= bit;
        let source = if self.persist.is_some() {
            "feed_time_persist_refused"
        } else {
            "feed_time_writer_missing"
        };
        let what = match cause {
            Cause::FormatReverted => "the day's hold",
            Cause::PricesOutsideSession | Cause::NoCandles => "today's latch and hold",
        };
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source,
            cause = cause.label(),
            %date,
            "The feed-time page could not hand {what} to its writer. Write \
             data/state/held-days/<date>.hold by hand before the evening archive runs."
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 20_777;

    fn run(ep: &mut FeedTimeEpisode, scans: &[(bool, u64, u64, u64, u64)]) -> Vec<Cause> {
        let mut totals = (0u64, 0u64, 0u64, 0u64);
        let mut out = Vec::new();
        for &(gate, ticks, bad, frames, seals) in scans {
            totals = (
                totals.0 + ticks,
                totals.1 + bad,
                totals.2 + frames,
                totals.3 + seals,
            );
            out.extend(ep.scan(DAY, gate, [false; 2], totals).into_iter().flatten());
        }
        out
    }

    #[test]
    fn test_regression_2026_11_20_shape_pages_cause_a_on_the_second_scan() {
        // Regression: 2026-11-20 — every index time read hours early, nothing
        // paged until 15:50. Two bad scans page once; later scans stay quiet.
        let mut ep = FeedTimeEpisode::new(DAY, [false; 2]);
        let fired = run(&mut ep, &[(true, 40, 40, 30, 0), (true, 40, 40, 30, 0)]);
        assert_eq!(fired, vec![Cause::PricesOutsideSession]);
        assert!(
            run(&mut ep, &[(true, 40, 40, 30, 0); 50])
                .iter()
                .all(|c| *c == Cause::NoCandles)
        );
    }

    #[test]
    fn test_cause_b_needs_twenty_scans_with_frames_and_no_candle() {
        let mut ep = FeedTimeEpisode::new(DAY, [false; 2]);
        assert!(run(&mut ep, &[(true, 0, 0, 10, 0); 19]).is_empty());
        assert_eq!(run(&mut ep, &[(true, 0, 0, 10, 0)]), vec![Cause::NoCandles]);
        assert!(
            run(&mut ep, &[(true, 0, 0, 10, 0); 40]).is_empty(),
            "once a day"
        );
        // A sealed candle resets the streak.
        let mut ep = FeedTimeEpisode::new(DAY, [false; 2]);
        let mut scans = vec![(true, 0, 0, 10, 0); 19];
        scans.push((true, 0, 0, 10, 1));
        scans.extend(vec![(true, 0, 0, 10, 0); 19]);
        assert!(run(&mut ep, &scans).is_empty());
        // No frames (feed quiet) is not "no candles".
        let mut ep = FeedTimeEpisode::new(DAY, [false; 2]);
        assert!(run(&mut ep, &[(true, 0, 0, 0, 0); 40]).is_empty());
    }

    #[test]
    fn test_cause_a_needs_twenty_ticks_half_bad_and_two_scans_in_a_row() {
        let mut ep = FeedTimeEpisode::new(DAY, [false; 2]);
        assert!(
            run(&mut ep, &[(true, 19, 19, 1, 1); 5]).is_empty(),
            "under 20 ticks"
        );
        assert!(
            run(&mut ep, &[(true, 40, 19, 1, 1); 5]).is_empty(),
            "under half"
        );
        assert!(
            run(
                &mut ep,
                &[
                    (true, 40, 40, 1, 1),
                    (true, 40, 0, 1, 1),
                    (true, 40, 40, 1, 1)
                ]
            )
            .is_empty(),
            "not two in a row"
        );
        assert_eq!(
            run(&mut ep, &[(true, 40, 20, 1, 1)]),
            vec![Cause::PricesOutsideSession],
            "exactly half, second in a row"
        );
    }

    #[test]
    fn test_outside_the_gate_never_fires_and_resets_streaks() {
        let mut ep = FeedTimeEpisode::new(DAY, [false; 2]);
        assert!(run(&mut ep, &[(false, 40, 40, 10, 0); 40]).is_empty());
        assert!(
            run(
                &mut ep,
                &[
                    (true, 40, 40, 10, 0),
                    (false, 40, 40, 10, 0),
                    (true, 40, 40, 10, 0)
                ]
            )
            .is_empty()
        );
    }

    #[test]
    fn test_a_restored_latch_never_pages_again_and_a_new_day_rearms() {
        let mut ep = FeedTimeEpisode::new(DAY, [true, true]);
        assert!(run(&mut ep, &[(true, 40, 40, 10, 0); 40]).is_empty());
        // The next day: the first scan only baselines, then it can fire.
        let totals = (1_000, 1_000, 1_000, 0);
        assert_eq!(ep.scan(DAY + 1, true, [false; 2], totals), [None, None]);
        let t2 = (1_040, 1_040, 1_010, 0);
        assert_eq!(ep.scan(DAY + 1, true, [false; 2], t2), [None, None]);
        let t3 = (1_080, 1_080, 1_020, 0);
        assert_eq!(
            ep.scan(DAY + 1, true, [false; 2], t3)[0],
            Some(Cause::PricesOutsideSession)
        );
    }

    fn page_with(tx: Option<SyncSender<(NaiveDate, Cause)>>) -> FeedTimePage {
        FeedTimePage::with_writer(DAY, [false; 2], tx)
    }

    #[test]
    fn test_regression_2026_11_21_a_reversion_page_holds_its_day_once() {
        // Regression: 2026-11-21 review — a reversion page (noise lock
        // §2.24-a) told the owner the day was held, and nothing held it
        // (daily-universe Quote 63). The scan now hands the page's day to
        // the writer thread, once a day, with no latch marker.
        let (tx, rx) = sync_channel(PERSIST_DEPTH);
        let mut page = page_with(Some(tx));
        let date = ist_day_date(DAY).expect("date");
        assert!(!page.hold_after_reversion_page(0, 0), "no page yet");
        assert!(rx.try_recv().is_err());
        assert!(page.hold_after_reversion_page(1, DAY));
        assert_eq!(rx.try_recv().ok(), Some((date, Cause::FormatReverted)));
        // Unchanged count: nothing more.
        assert!(!page.hold_after_reversion_page(1, DAY));
        // The second segment pages the same day: already held.
        assert!(!page.hold_after_reversion_page(2, DAY));
        assert!(rx.try_recv().is_err());
        // A page on the next day holds that day.
        assert!(page.hold_after_reversion_page(3, DAY + 1));
        assert_eq!(
            rx.try_recv().ok(),
            Some((ist_day_date(DAY + 1).expect("date"), Cause::FormatReverted))
        );
        // The hold carries no latch marker and names the reason.
        assert_eq!(Cause::FormatReverted.task(), None);
        assert_eq!(Cause::FormatReverted.label(), "dhan_time_format_reverted");
        assert_eq!(Cause::PricesOutsideSession.task(), Some(TASK_CAUSE_A));
        // With no writer thread nothing is handed off: it is not marked
        // held and every later scan tries again (review fix 2026-11-21: it
        // used to return true and mark the day held silently).
        let mut lonely = page_with(None);
        assert!(!lonely.hold_after_reversion_page(1, DAY));
        assert!(!lonely.hold_after_reversion_page(1, DAY), "retried");
        assert_eq!(lonely.reversion_pending_day, Some(DAY));
        assert_ne!(lonely.reversion_held_day, DAY);
        assert_eq!(
            lonely.handoff_failures_logged,
            (DAY, 1 << Cause::FormatReverted.index()),
            "the failure is logged once per cause per day"
        );
    }

    #[test]
    fn test_regression_2026_11_21_a_full_writer_queue_holds_on_the_next_scan() {
        // A hand-off refused by a full queue is tried again at the next scan
        // and the day is held once there is room.
        let (tx, rx) = sync_channel(1);
        tx.try_send((ist_day_date(DAY).expect("date"), Cause::NoCandles))
            .expect("fill");
        let mut page = page_with(Some(tx));
        assert!(!page.hold_after_reversion_page(1, DAY), "queue full");
        assert_eq!(page.reversion_pending_day, Some(DAY));
        assert!(rx.try_recv().is_ok(), "drain the earlier hand-off");
        assert!(
            page.hold_after_reversion_page(1, DAY),
            "handed off on retry"
        );
        assert_eq!(
            rx.try_recv().ok(),
            Some((ist_day_date(DAY).expect("date"), Cause::FormatReverted))
        );
        assert_eq!(page.reversion_pending_day, None);
        assert!(!page.hold_after_reversion_page(1, DAY), "once a day");
    }

    #[test]
    fn test_fire_is_never_called_with_the_reversion_cause() {
        // §2.24-a REJECT: no second DHAN-FEED-TIME-01 line from the hold
        // hand-off. Only the scan's A/B loop calls `fire`, and the reversion
        // cause is handed off through `hold_after_reversion_page`.
        let src = include_str!("feed_time_page.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("tests")];
        let calls: Vec<&str> = prod
            .match_indices("self.fire(")
            .map(|(i, _)| &prod[i..prod[i..].find(';').map_or(prod.len(), |e| i + e)])
            .collect();
        assert_eq!(calls, vec!["self.fire(day, cause)"], "{calls:?}");
        assert!(!prod.contains("fire(day, Cause::FormatReverted)"));
        let scan = &prod[prod
            .find("pub(super) fn scan(&mut self, ingest")
            .expect("scan")..];
        assert!(scan.contains("for cause in fired.into_iter().flatten()"));
    }

    #[test]
    fn test_scan_reads_the_guard_page_count_for_the_hold() {
        // The scan, the only caller, wires the guard's page count and day in.
        let src = include_str!("feed_time_page.rs");
        let scan = &src[src
            .find("pub(super) fn scan(&mut self, ingest")
            .expect("scan")..];
        let scan = &scan[..scan.find("\n    }\n").expect("end")];
        assert!(scan.contains("let ltt = ingest.ltt.counts();"), "{scan}");
        assert!(
            scan.contains(
                "self.hold_after_reversion_page(ltt.reversion_pages, ltt.last_reversion_day);"
            ),
            "{scan}"
        );
    }

    #[test]
    fn test_tally_judges_corrected_times_against_the_window_and_receipt() {
        // 2026-11-20 11:00:00 IST as a UTC receipt.
        let receipt = 1_795_152_600 * 1_000_000_000;
        let corrected = 1_795_152_600 + 19_800;
        let mut t = FeedTimeTally::default();
        t.note_index_tick(corrected as u32, receipt);
        assert_eq!(
            (t.index_ticks, t.index_bad),
            (1, 0),
            "a corrected time is good"
        );
        // The raw UTC time (05:30 read as IST) is outside the session.
        t.note_index_tick(1_795_152_600, receipt);
        assert_eq!(t.index_bad, 1);
        // In the session but 6 minutes from receipt.
        t.note_index_tick((corrected - 360) as u32, receipt);
        assert_eq!(t.index_bad, 2);
        t.note_frame();
        assert_eq!(t.main_frames, 1);
    }
}
