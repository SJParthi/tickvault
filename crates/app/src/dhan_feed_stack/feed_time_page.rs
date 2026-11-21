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
/// Daily-marker task names (one per cause), kept 7 days. Shared with the
/// trade-safety switch, whose chain start reads them (plan item UTC-SAFETY).
const TASK_CAUSE_A: &str = crate::trade_safety::FEED_TIME_PAGE_TASKS[0].0;
const TASK_CAUSE_B: &str = crate::trade_safety::FEED_TIME_PAGE_TASKS[1].0;
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

    /// The trade-safety cause bit this cause engages.
    const fn safety_bit(self) -> u8 {
        match self {
            Self::PricesOutsideSession => crate::trade_safety::CAUSE_PRICES_OUTSIDE_SESSION,
            Self::NoCandles => crate::trade_safety::CAUSE_NO_CANDLES,
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

/// What the drain hands the writer thread (plan item UTC-SAFETY: every step
/// beyond the switch's one store runs THERE, never on the frame drain,
/// daily-universe §28.7; 2026-11-21 review finding 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistMsg {
    /// A cause fired on `day`. `changed`: the drain's store changed the
    /// switch, so the writer logs the switch's `warn!` and sets its gauge.
    Fired {
        date: NaiveDate,
        day: i64,
        cause: Cause,
        changed: bool,
    },
    /// The drain's scan saw a new IST day: the writer re-engages the switch
    /// from that day's markers and hold (strict, fail closed) and sets the
    /// gauge from the switch for the new day.
    NewDay { day: i64 },
}

/// Slots of the drain's pending hand-offs: one per cause, one for the day
/// change. A refused hand-off waits here and is offered again on each scan.
const PENDING_SLOTS: usize = 3;
const PENDING_NEW_DAY: usize = 2;
/// Tries of the daily-marker write on the writer thread, and the pause
/// between them (2026-11-21 review finding 2: a marker that never lands left
/// the switch open after a restart).
const MARKER_WRITE_ATTEMPTS: u32 = 3;
const MARKER_WRITE_PAUSE_MS: u64 = 200;

/// The drain-side page: the episode plus the hand-off to the writer thread.
#[derive(Debug)]
pub(super) struct FeedTimePage {
    episode: FeedTimeEpisode,
    restored_day: i64,
    restored: [bool; 2],
    persist: Option<SyncSender<PersistMsg>>,
    /// Hand-offs the writer thread refused (queue full or thread gone),
    /// offered again on each scan: `try_send` only, O(1), no allocation.
    pending: [Option<PersistMsg>; PENDING_SLOTS],
    /// The trade-safety switch a firing cause engages (plan item
    /// UTC-SAFETY): the process switch in production, a test's own in tests.
    safety: &'static crate::trade_safety::SafetySwitch,
}

/// Engages the trade-safety switch for `day` from that day's two feed-time
/// page markers, read with the STRICT reader `state_of` (production:
/// `daily_task_marker::marker_state`: a marker that is present, or that is
/// there but cannot be trusted, blocks), and from that day's `.hold` file when
/// it is the page's own (`hold_of`, production:
/// `trade_safety::process_feed_time_hold_bits`), so a restart never fails open (daily-universe §28.7;
/// 2026-11-21 reviews: the fail-open latch reader, then a marker write that
/// failed while the hold landed). Then sets the gauge from the switch for
/// `day`. Returns the gauge value. Cold: three small file reads. Runs at the
/// drain's start (before its loop) and on the WRITER thread at each IST day
/// change, never inside the drain's loop (it logs and takes the metrics
/// registry lock).
fn engage_for_day(
    safety: &crate::trade_safety::SafetySwitch,
    day: i64,
    state_of: impl Fn(&str, NaiveDate) -> crate::daily_task_marker::MarkerState,
    hold_of: impl Fn(NaiveDate) -> u8,
) -> f64 {
    let _bits = crate::live_fill::engage_from_feed_time_markers(safety, day, state_of);
    let _hold = crate::live_fill::engage_from_feed_time_hold(safety, day, hold_of);
    crate::trade_safety::publish_gauge(safety, day)
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

/// Runs `write` up to `attempts` times (at least once), pausing `pause`
/// between tries, and returns the last error when none landed. Cold, the
/// writer thread only.
fn write_with_retries<T>(
    attempts: u32,
    pause: std::time::Duration,
    mut write: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut tries = 0u32;
    loop {
        tries = tries.saturating_add(1);
        match write() {
            Ok(v) => return Ok(v),
            Err(err) if tries >= attempts.max(1) => return Err(err),
            Err(_) => std::thread::sleep(pause),
        }
    }
}

fn run_persist(rx: &Receiver<PersistMsg>, safety: &'static crate::trade_safety::SafetySwitch) {
    while let Ok(msg) = rx.recv() {
        match msg {
            PersistMsg::NewDay { day } => {
                let _gauge = engage_for_day(
                    safety,
                    day,
                    crate::daily_task_marker::marker_state,
                    crate::trade_safety::process_feed_time_hold_bits,
                );
            }
            PersistMsg::Fired {
                date,
                day,
                cause,
                changed,
            } => persist_fired(safety, date, day, cause, changed),
        }
    }
}

/// The writer thread's half of a firing cause: the switch's `warn!` and gauge
/// when the drain's store changed it, then the daily marker (retried) and the
/// day's hold. Cold.
fn persist_fired(
    safety: &crate::trade_safety::SafetySwitch,
    date: NaiveDate,
    day: i64,
    cause: Cause,
    changed: bool,
) {
    if changed {
        crate::trade_safety::report_engaged(safety, day);
    }
    if let Err(err) = write_with_retries(
        MARKER_WRITE_ATTEMPTS,
        std::time::Duration::from_millis(MARKER_WRITE_PAUSE_MS),
        || crate::daily_task_marker::try_write_daily_marker(cause.task(), date),
    ) {
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "feed_time_latch_write_failed",
            cause = cause.label(),
            %err,
            "Could not save today's feed-time page latch; a restart today may page again."
        );
    }
    let reason = format!(
        "{}{}",
        crate::trade_safety::FEED_TIME_HOLD_REASON_PREFIX,
        cause.label()
    );
    if let Err(err) = write_with_retries(
        MARKER_WRITE_ATTEMPTS,
        std::time::Duration::from_millis(MARKER_WRITE_PAUSE_MS),
        || {
            tickvault_storage::held_days::write_day_marker(
                std::path::Path::new(tickvault_storage::held_days::HELD_DAYS_DIR),
                tickvault_storage::held_days::MarkerKind::Hold,
                date,
                tickvault_storage::held_days::today_ist(),
                &reason,
            )
        },
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

impl FeedTimePage {
    /// Restores today's latches and starts the writer thread. Cold: once per
    /// drain, before its loop (five small file reads: the two markers and the
    /// hold for the switch, the two markers again for the page's latch).
    pub(super) fn start() -> Self {
        Self::start_with(
            super::ist_day_number_now(),
            &crate::trade_safety::FEED_TIME_BLOCK,
            crate::daily_task_marker::marker_state,
            crate::trade_safety::process_feed_time_hold_bits,
        )
    }

    /// [`Self::start`] for a given day, switch, strict marker reader and hold
    /// reader (the test seam). Engages the switch from the strict reads before
    /// anything else, then restores the page's latch with its own reader.
    fn start_with(
        day: i64,
        safety: &'static crate::trade_safety::SafetySwitch,
        state_of: impl Fn(&str, NaiveDate) -> crate::daily_task_marker::MarkerState,
        hold_of: impl Fn(NaiveDate) -> u8,
    ) -> Self {
        let _gauge = engage_for_day(safety, day, state_of, hold_of);
        let restored = restore_latch(day);
        let (tx, rx) = sync_channel(PERSIST_DEPTH);
        let persist = std::thread::Builder::new()
            .name("tv-feed-time-latch".to_string())
            .spawn(move || run_persist(&rx, safety))
            .ok()
            .map(|_| tx);
        Self {
            episode: FeedTimeEpisode::new(day, restored),
            restored_day: day,
            restored,
            persist,
            pending: [None; PENDING_SLOTS],
            safety,
        }
    }

    /// One 30 s scan on the drain's silence arm. O(1); a day change reads the
    /// new day's two latch markers once (cold) and hands the switch's re-read
    /// to the writer thread.
    pub(super) fn scan(&mut self, ingest: &mut LiveIngest) {
        let now_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let day = receipt_ist_day(now_nanos);
        if day != self.restored_day {
            self.on_day_change(day);
        }
        self.offer_pending();
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

    /// The scan's IST day change, on the drain: restores the page's latch
    /// with its own reader (the two file reads the page always made here) and
    /// queues the switch's re-read for the writer thread (strict markers and
    /// hold, fail closed, and the gauge from the switch for the new day, 0
    /// once yesterday's block has ended). No log, no metrics, no strict read
    /// on the drain (§28.7; 2026-11-21 review finding 1). Cold.
    fn on_day_change(&mut self, day: i64) {
        self.restored_day = day;
        self.restored = restore_latch(day);
        if let Some(slot) = self.pending.get_mut(PENDING_NEW_DAY) {
            *slot = Some(PersistMsg::NewDay { day });
        }
        self.offer_pending();
    }

    /// Offers every waiting hand-off to the writer thread once. `try_send`
    /// only, at most [`PENDING_SLOTS`], O(1), no allocation, never a wait. A
    /// slot stays until the writer takes it (a writer that never started or
    /// has gone keeps refusing; that refusal was logged when it first
    /// happened).
    fn offer_pending(&mut self) {
        let Some(tx) = &self.persist else {
            return;
        };
        for slot in &mut self.pending {
            if let Some(msg) = *slot
                && tx.try_send(msg).is_ok()
            {
                *slot = None;
            }
        }
    }

    fn fire(&mut self, day: i64, cause: Cause) {
        // FIRST (plan item UTC-SAFETY): block today's paper entries before
        // anything that can fail, so a refused hand-off or a failed marker
        // write still blocks them in this process. ONE store on the drain
        // (§28.7); the switch's log and gauge run on the writer thread.
        let changed = self.safety.engage(day, cause.safety_bit());
        error!(
            code = ErrorCode::DhanFeedTime01TradeTimesWrong.code_str(),
            cause = cause.label(),
            trade_safety_engaged = true,
            "Dhan feed times look wrong during the session: {}. Today is held out of the \
             archive and the export until it is repaired; raw data is kept. New paper \
             entries are blocked for the rest of today; exits still go out.",
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
        let msg = PersistMsg::Fired {
            date,
            day,
            cause,
            changed,
        };
        let sent = self
            .persist
            .as_ref()
            .is_some_and(|tx| tx.try_send(msg).is_ok());
        if !sent {
            if let Some(slot) = self.pending.get_mut(cause.index()) {
                *slot = Some(msg);
            }
            error!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "feed_time_persist_refused",
                cause = cause.label(),
                "The feed-time page could not hand today's latch and hold to its writer; it \
                 retries on each scan."
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

    fn leaked_switch() -> &'static crate::trade_safety::SafetySwitch {
        Box::leak(Box::new(crate::trade_safety::SafetySwitch::new()))
    }

    fn page(
        safety: &'static crate::trade_safety::SafetySwitch,
        persist: Option<SyncSender<PersistMsg>>,
    ) -> FeedTimePage {
        FeedTimePage {
            episode: FeedTimeEpisode::new(DAY, [false; 2]),
            restored_day: DAY,
            restored: [false; 2],
            persist,
            pending: [None; PENDING_SLOTS],
            safety,
        }
    }

    fn no_hold(_: NaiveDate) -> u8 {
        0
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "tv_feed_time_page_{tag}_{}_{}",
            std::process::id(),
            DAY
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("dir");
        base
    }

    #[test]
    fn test_fire_engages_the_trade_safety_switch_with_no_writer_and_a_refused_hand_off() {
        // No writer thread at all (spawn failed): entries are still blocked,
        // and the hand-off waits in its slot.
        let safety = leaked_switch();
        let mut p = page(safety, None);
        assert_eq!(safety.entries_blocked_on(DAY), None);
        p.fire(DAY, Cause::PricesOutsideSession);
        assert_eq!(
            safety.entries_blocked_on(DAY),
            Some(crate::trade_safety::CAUSE_PRICES_OUTSIDE_SESSION)
        );
        assert_eq!(p.restored, [true, false]);
        assert!(matches!(
            p.pending[0],
            Some(PersistMsg::Fired {
                cause: Cause::PricesOutsideSession,
                changed: true,
                ..
            })
        ));
        // A writer whose receiver is gone refuses the hand-off: still blocked,
        // and the second cause ORs in.
        let safety = leaked_switch();
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let mut p = page(safety, Some(tx));
        p.fire(DAY, Cause::NoCandles);
        assert_eq!(
            safety.entries_blocked_on(DAY),
            Some(crate::trade_safety::CAUSE_NO_CANDLES)
        );
        p.fire(DAY, Cause::PricesOutsideSession);
        assert_eq!(safety.entries_blocked_on(DAY), Some(3));
        assert_eq!(
            safety.entries_blocked_on(DAY + 1),
            None,
            "the next day is not blocked"
        );
    }

    /// Regression: 2026-11-21 (review finding 1) — `fire` and the day change
    /// ran the switch's `warn!`, its gauge and strict marker reads ON the
    /// frame drain (§28.7 allows only the one store there). The drain now
    /// hands both to the writer thread: `fire` sends `changed`, the day change
    /// sends `NewDay`, and a refused hand-off is offered again on each scan.
    #[test]
    fn test_regression_2026_11_21_drain_hands_the_report_and_day_change_to_the_writer() {
        let safety = leaked_switch();
        let (tx, rx) = sync_channel(1);
        let mut p = page(safety, Some(tx));
        p.fire(DAY, Cause::NoCandles);
        let date = ist_day_date(DAY).expect("date");
        assert_eq!(
            rx.try_recv(),
            Ok(PersistMsg::Fired {
                date,
                day: DAY,
                cause: Cause::NoCandles,
                changed: true
            })
        );
        // The same cause again in the same day changes nothing.
        p.fire(DAY, Cause::NoCandles);
        assert!(matches!(
            rx.try_recv(),
            Ok(PersistMsg::Fired { changed: false, .. })
        ));
        // Fill the one-slot queue, then a day change: the NewDay waits in
        // its slot and is delivered on the next offer once there is room.
        p.fire(DAY, Cause::PricesOutsideSession);
        p.on_day_change(DAY + 1);
        assert_eq!(p.restored_day, DAY + 1);
        assert_eq!(
            p.pending[PENDING_NEW_DAY],
            Some(PersistMsg::NewDay { day: DAY + 1 })
        );
        assert!(matches!(rx.try_recv(), Ok(PersistMsg::Fired { .. })));
        p.offer_pending();
        assert_eq!(p.pending[PENDING_NEW_DAY], None);
        assert_eq!(rx.try_recv(), Ok(PersistMsg::NewDay { day: DAY + 1 }));
        // A refused Fired waits in its cause's slot and is retried.
        p.fire(DAY + 1, Cause::NoCandles);
        p.fire(DAY + 1, Cause::PricesOutsideSession);
        assert!(p.pending[0].is_some(), "queue full: cause A waits");
        let _ = rx.try_recv();
        p.offer_pending();
        assert!(p.pending[0].is_none());
        assert!(matches!(
            rx.try_recv(),
            Ok(PersistMsg::Fired {
                cause: Cause::PricesOutsideSession,
                ..
            })
        ));
    }

    #[test]
    fn test_engage_for_day_blocks_on_present_or_unreadable_markers_only() {
        use crate::daily_task_marker::MarkerState;
        let safety = leaked_switch();
        let absent = |_: &str, _: NaiveDate| MarkerState::Absent;
        assert_eq!(engage_for_day(safety, DAY, absent, no_hold), 0.0);
        assert_eq!(safety.entries_blocked_on(DAY), None);
        let b_present = |t: &str, _: NaiveDate| {
            if t == TASK_CAUSE_B {
                MarkerState::Present
            } else {
                MarkerState::Absent
            }
        };
        assert_eq!(engage_for_day(safety, DAY, b_present, no_hold), 1.0);
        assert_eq!(
            safety.entries_blocked_on(DAY),
            Some(crate::trade_safety::CAUSE_NO_CANDLES)
        );
        // A day change with nothing on disk for the new day: clear, gauge 0.
        assert_eq!(engage_for_day(safety, DAY + 1, absent, no_hold), 0.0);
        assert_eq!(safety.entries_blocked_on(DAY + 1), None);
        let both = |_: &str, _: NaiveDate| MarkerState::Unreadable;
        assert_eq!(engage_for_day(safety, DAY + 1, both, no_hold), 1.0);
        assert_eq!(safety.entries_blocked_on(DAY + 1), Some(3));
        assert_eq!(safety.entries_blocked_on(DAY), None);
        // The hold alone blocks too.
        let safety = leaked_switch();
        let a_hold = |_: NaiveDate| crate::trade_safety::CAUSE_PRICES_OUTSIDE_SESSION;
        assert_eq!(engage_for_day(safety, DAY, absent, a_hold), 1.0);
        assert_eq!(
            safety.entries_blocked_on(DAY),
            Some(crate::trade_safety::CAUSE_PRICES_OUTSIDE_SESSION)
        );
    }

    /// Regression: 2026-11-21 (review finding 2) — the page fired, the hand-off
    /// was refused (or the marker write failed) but the day's `.hold` landed;
    /// after a restart both restore paths read only the markers and found
    /// nothing, so entries reopened. The start now also reads the hold.
    #[test]
    fn test_regression_2026_11_21_hold_without_marker_blocks_after_restart() {
        use crate::daily_task_marker::MarkerState;
        let base = temp_dir("hold_only");
        let date = ist_day_date(DAY).expect("date");
        // The page fires with no writer: blocked in memory only.
        let safety = leaked_switch();
        let mut p = page(safety, None);
        p.fire(DAY, Cause::NoCandles);
        assert!(safety.entries_blocked_on(DAY).is_some());
        // The hold landed (as the writer thread writes it); no marker exists.
        tickvault_storage::held_days::write_day_marker(
            &base,
            tickvault_storage::held_days::MarkerKind::Hold,
            date,
            date,
            &format!(
                "{}{}",
                crate::trade_safety::FEED_TIME_HOLD_REASON_PREFIX,
                Cause::NoCandles.label()
            ),
        )
        .expect("hold");
        // Restart: a fresh process switch.
        let restarted = leaked_switch();
        let absent = |_: &str, _: NaiveDate| MarkerState::Absent;
        let hold = |d: NaiveDate| crate::trade_safety::feed_time_hold_bits(&base, d);
        let p = FeedTimePage::start_with(DAY, restarted, absent, hold);
        assert_eq!(
            restarted.entries_blocked_on(DAY),
            Some(crate::trade_safety::CAUSE_NO_CANDLES),
            "a feed-time hold must block entries after a restart"
        );
        drop(p);
        // The chain start's path reads it the same way.
        let chain_switch = leaked_switch();
        assert_eq!(
            crate::live_fill::engage_from_feed_time_hold(chain_switch, DAY, hold),
            crate::trade_safety::CAUSE_NO_CANDLES
        );
        assert!(chain_switch.entries_blocked_on(DAY).is_some());
        // A hold for another day blocks nothing today.
        let other = leaked_switch();
        assert_eq!(
            crate::live_fill::engage_from_feed_time_hold(other, DAY + 1, hold),
            0
        );
        assert_eq!(other.entries_blocked_on(DAY + 1), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_write_with_retries_tries_up_to_the_limit() {
        let pause = std::time::Duration::ZERO;
        let mut calls = 0u32;
        let ok = write_with_retries(3, pause, || {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::other("full"))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(ok.ok(), Some(3));
        let mut calls = 0u32;
        let err = write_with_retries(3, pause, || -> std::io::Result<()> {
            calls += 1;
            Err(std::io::Error::other("full"))
        });
        assert!(err.is_err());
        assert_eq!(calls, 3);
        // Zero attempts still tries once.
        let mut calls = 0u32;
        let _ = write_with_retries(0, pause, || -> std::io::Result<()> {
            calls += 1;
            Err(std::io::Error::other("full"))
        });
        assert_eq!(calls, 1);
    }

    /// Regression: 2026-11-21 (review finding 1 of the first round) — the
    /// drain's page start re-engaged the switch only through `restore_latch`,
    /// which reads the fail-OPEN `daily_marker_exists`, so a torn
    /// `feed-time-page-a` marker left entries open after a restart when no
    /// chain ran. The start now reads the markers with the strict reader: a
    /// torn marker on a real folder blocks. The day change's re-read (on the
    /// writer thread since the second round) uses the same strict path.
    #[test]
    fn test_regression_2026_11_21_torn_feed_time_marker_blocks_at_page_start() {
        let base = temp_dir("torn");
        let date = ist_day_date(DAY).expect("date");
        // A torn (empty) cause-A marker: the fail-open reader says absent.
        std::fs::write(base.join(format!("{TASK_CAUSE_A}-{date}.marker")), b"").expect("torn");
        assert!(!crate::daily_task_marker::test_marker_exists_in(
            &base,
            TASK_CAUSE_A,
            date
        ));
        let read =
            |t: &str, d: NaiveDate| crate::daily_task_marker::test_marker_state_in(&base, t, d);
        let safety = leaked_switch();
        let p = FeedTimePage::start_with(DAY, safety, read, no_hold);
        assert_eq!(
            safety.entries_blocked_on(DAY),
            Some(crate::trade_safety::CAUSE_PRICES_OUTSIDE_SESSION),
            "a torn marker must block entries (fail closed)"
        );
        drop(p);
        // The writer thread's NewDay path for the next day.
        let safety = leaked_switch();
        let date2 = ist_day_date(DAY + 1).expect("date");
        std::fs::write(base.join(format!("{TASK_CAUSE_B}-{date2}.marker")), b"torn").expect("torn");
        assert_eq!(engage_for_day(safety, DAY + 1, read, no_hold), 1.0);
        assert_eq!(
            safety.entries_blocked_on(DAY + 1),
            Some(crate::trade_safety::CAUSE_NO_CANDLES)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression: 2026-11-21 (review finding 2 of the first round) — the
    /// entries-blocked gauge had two writers that disagreed: with no chain
    /// running it stayed 1 after the IST day roll. The writer thread's day
    /// change writes it from the switch for the new day. Read through a real
    /// Prometheus recorder.
    #[test]
    fn test_regression_2026_11_21_gauge_reads_zero_after_the_day_roll() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let safety = leaked_switch();
        let absent = |_: &str, _: NaiveDate| crate::daily_task_marker::MarkerState::Absent;
        let line = |text: &str| {
            text.lines()
                .find(|l| l.starts_with(crate::trade_safety::ENTRIES_BLOCKED_GAUGE))
                .map(str::to_owned)
        };
        metrics::with_local_recorder(&recorder, || {
            // Day D: the page fires, the engage edge sets the gauge to 1.
            assert!(safety.engage(DAY, crate::trade_safety::CAUSE_NO_CANDLES));
            assert_eq!(crate::trade_safety::publish_gauge(safety, DAY), 1.0);
        });
        assert_eq!(
            line(&handle.render()).as_deref(),
            Some("tv_trade_safety_entries_blocked 1")
        );
        metrics::with_local_recorder(&recorder, || {
            // The writer's NewDay on day D+1: nothing on disk for D+1, so 0.
            assert_eq!(engage_for_day(safety, DAY + 1, absent, no_hold), 0.0);
        });
        assert_eq!(
            line(&handle.render()).as_deref(),
            Some("tv_trade_safety_entries_blocked 0")
        );
        assert_eq!(safety.entries_blocked_on(DAY + 1), None);
    }

    #[test]
    fn test_page_marker_tasks_and_bits_match_the_trade_safety_table() {
        for (cause, i) in [(Cause::PricesOutsideSession, 0), (Cause::NoCandles, 1)] {
            assert_eq!(cause.task(), crate::trade_safety::FEED_TIME_PAGE_TASKS[i].0);
            assert_eq!(
                cause.safety_bit(),
                crate::trade_safety::FEED_TIME_PAGE_TASKS[i].1
            );
        }
    }
}
