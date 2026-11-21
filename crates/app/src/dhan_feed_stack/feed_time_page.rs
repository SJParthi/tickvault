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
const MUHURAT_WINDOW_LEAD_SECS: u32 = 15 * 60;
const MUHURAT_WINDOW_TAIL_SECS: u32 = 10 * 60;
/// Daily-marker task names (one per cause), kept 7 days.
const TASK_CAUSE_A: &str = "feed-time-page-a";
const TASK_CAUSE_B: &str = "feed-time-page-b";
/// Hand-offs the writer thread can hold (two a day at most).
const PERSIST_DEPTH: usize = 4;

/// Which cause fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cause {
    /// Trade times outside the session after the correction.
    PricesOutsideSession,
    /// Frames arriving and no candle sealed for 10 minutes.
    NoCandles,
}

impl Cause {
    const fn index(self) -> usize {
        match self {
            Self::PricesOutsideSession => 0,
            Self::NoCandles => 1,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::PricesOutsideSession => "prices_outside_session",
            Self::NoCandles => "no_candles",
        }
    }

    const fn task(self) -> &'static str {
        match self {
            Self::PricesOutsideSession => TASK_CAUSE_A,
            Self::NoCandles => TASK_CAUSE_B,
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
        if let Err(err) = crate::daily_task_marker::try_write_daily_marker(cause.task(), date) {
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
        let persist = std::thread::Builder::new()
            .name("tv-feed-time-latch".to_string())
            .spawn(move || run_persist(&rx))
            .ok()
            .map(|_| tx);
        Self {
            episode: FeedTimeEpisode::new(day, restored),
            restored_day: day,
            restored,
            persist,
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
    }

    fn fire(&mut self, day: i64, cause: Cause) {
        error!(
            code = ErrorCode::DhanFeedTime01TradeTimesWrong.code_str(),
            cause = cause.label(),
            "Dhan feed times look wrong during the session: {}. Today is held out of the \
             archive and the export until it is repaired; raw data is kept.",
            match cause {
                Cause::PricesOutsideSession =>
                    "most index prices carry a time outside market hours (after our correction)",
                Cause::NoCandles => "no candle has formed for 10 minutes while prices arrive",
            }
        );
        if let Some(index) = self.restored.get_mut(cause.index()) {
            *index = true;
        }
        let Some(date) = ist_day_date(day) else {
            return;
        };
        if let Some(tx) = &self.persist
            && tx.try_send((date, cause)).is_err()
        {
            error!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "feed_time_persist_refused",
                cause = cause.label(),
                "The feed-time page could not hand today's latch and hold to its writer."
            );
        }
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
