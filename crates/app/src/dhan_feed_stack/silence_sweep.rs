//! The drain's 30 s silence check, split into small idle steps (plan item
//! US1-D, 2026-11-24, owner card "Split into small pieces (Recommended)").
//!
//! Until this change the 30 s timer arm scanned every tracked instrument in
//! one go (measured release, x86 dev container, 20,000 instruments: median
//! 185 µs warm, 436 µs cold, one 1.95 ms maximum), and every frame that
//! arrived meanwhile waited in the ring. Now:
//!
//! * the timer arm calls [`LiveIngest::begin_silence_sweep`], **O(1)**: it
//!   freezes the scan instant, the reconcile instant and clock, the session
//!   gate's seconds, the trading-day flag and the tracked-slot count (`end`),
//!   and opens a chunked reconcile pass;
//! * the drain's idle arm runs one [`LiveIngest::step_silence_sweep`] at a
//!   time, [`SILENCE_SWEEP_STEP_SLOTS`] slots per step, **O(step) per idle
//!   step, plus the finish on the last step**, taking turns with the
//!   catch-up seal while both are pending;
//! * the step that reaches `end` also runs the finish: the reconcile finish
//!   (O(sockets + candidates), O(expected) on an IST day change), the two
//!   gauges and the dead-class verdict, and hands a [`SilenceDone`] to
//!   [`SilencePager::judge`], which runs the page logic that used to sit at
//!   the end of the timer arm, unchanged in text, codes, fields and order,
//!   on the frozen values.
//!
//! A timer that fires with a sweep still pending (the drain had no idle time
//! for 30 s) drops that stale sweep and its reconcile pass, counts it on
//! [`SILENCE_SWEEP_OVERRUN_COUNTER`], and runs the new sweep to its end at
//! once on the fresh instant, returning it for the page. So every timer
//! period still yields exactly one judged sweep, on fresh data, and the
//! worst case is the old one-go stop, counted.
//!
//! Same counts as the one-go scan for the same instant when nothing touches
//! the detector or the reconcile book between steps (proptest
//! `proptest_sliced_silence_sweep_matches_the_one_go_scan`). Honest limit:
//! a tick or a re-seed on a slot not yet visited is judged at the begin
//! instant on its newer state (a slot that ticked after the begin reads
//! live, which it is), and slots added mid-sweep wait for the next sweep, as
//! a one-go scan at the begin instant would leave them out. A reconcile lock
//! taker between steps makes that sweep plan no round (counted by the book).

use std::time::Instant;

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

use super::silence_tally::SilenceTally;
use super::{
    INSTRUMENTS_NEVER_TICKED_GAUGE, INSTRUMENTS_SILENT_GAUGE, LiveIngest,
    SILENCE_SCANS_BEFORE_ALERT, SilentInstrument, WORST_SILENT_NAMED, dead_class_verdict_is_due,
    is_within_market_hours_ist, ist_secs_of_day_from_millis, muhurat_silence_check_due_now,
    now_ist_secs_of_day, silence_page_allowed_today, silence_page_floor, silence_page_is_cooling,
    silence_scan_pending, silence_warrants_page,
};

/// Detector slots one idle step visits. At the measured 97 to 436 µs per
/// full 20,000-instrument arm, one step MEASURED 9.5 µs warm and 22.5 µs
/// cold (medians, release `silence_arm_cost` harness, x86 dev container,
/// 2026-11-24); not measured on the prod host.
pub const SILENCE_SWEEP_STEP_SLOTS: u32 = 1_024;

/// Counter: 30 s timer fires that found the previous silence sweep still
/// pending (the drain had no idle time for 30 s), dropped it as stale and
/// ran the new sweep to its end at once.
/// Local `/metrics` only, registered at 0 when the ingest is built; no alarm,
/// filter or EMF name.
pub const SILENCE_SWEEP_OVERRUN_COUNTER: &str = "tv_silence_sweep_overrun_total";

/// Shortest gap between two RISK-GAP-03 pages, however many separate
/// silence episodes occur inside it. See `last_silence_page` in
/// [`SilencePager`].
pub(super) const SILENCE_PAGE_COOLDOWN_SECS: u64 = 1_800;

/// One silence sweep part-way through the detector's slots. Everything the
/// finish reads is frozen at the begin, so the finish judges the begin
/// instant however many steps later it runs.
#[derive(Debug, Clone, Copy)]
struct SilenceSweep {
    /// The scan instant (unix millis): every report and the dead-class
    /// verdict are judged at it.
    now_millis: u64,
    /// The reconcile's `(IST seconds of day, IST day)` for its finish.
    reconcile_clock: (u32, i64),
    /// Whether this sweep holds a chunked reconcile pass.
    reconcile: bool,
    /// IST seconds of day for the page's session gate.
    gate_secs: u64,
    /// Today is a trading day (or the calendar is absent: fail open).
    page_allowed_today: bool,
    /// Tracked slots at the begin; slots added later wait for the next sweep.
    end: u32,
    /// The next slot to visit.
    cursor: u32,
    tally: SilenceTally,
    worst: [SilentInstrument; WORST_SILENT_NAMED],
}

/// What a finished silence sweep found, for [`SilencePager::judge`]. `Copy`
/// and scalar-only, so handing it from the idle arm allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilenceDone {
    /// Alarm-worthy silent instruments.
    pub silent: u64,
    /// Of those, never ticked since being subscribed.
    pub never: u64,
    /// How many entries of `worst` are filled.
    pub named: usize,
    /// The quietest instruments, descending by silence.
    pub worst: [SilentInstrument; WORST_SILENT_NAMED],
    /// The frozen scan instant (unix millis).
    pub now_millis: u64,
    /// The frozen IST seconds of day for the session gate.
    pub gate_secs: u64,
    /// The frozen trading-day flag.
    pub page_allowed_today: bool,
    /// Tracked slots at the begin (the sweep's `end`).
    pub tracked: u64,
}

/// The ingest's silence-sweep state: the pending sweep and its overrun count.
#[derive(Debug)]
pub(super) struct SilenceSweepSlot {
    pending: Option<SilenceSweep>,
    overruns: u64,
    overrun_counter: metrics::Counter,
    /// Slots per step; [`SILENCE_SWEEP_STEP_SLOTS`] outside tests.
    step_slots: u32,
    /// Whose turn the next idle step is when the catch-up seal and a silence
    /// sweep are both pending: `true` is the silence sweep.
    silence_turn: bool,
}

impl SilenceSweepSlot {
    /// No sweep pending; the overrun counter registered at 0.
    pub(super) fn new() -> Self {
        let overrun_counter = metrics::counter!(SILENCE_SWEEP_OVERRUN_COUNTER);
        overrun_counter.increment(0);
        Self {
            pending: None,
            overruns: 0,
            overrun_counter,
            step_slots: SILENCE_SWEEP_STEP_SLOTS,
            silence_turn: false,
        }
    }

    /// Whether a sweep waits for idle steps. O(1).
    pub(super) const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The idle arm's choice when the catch-up seal and a silence sweep are
    /// both pending: returns whether this step goes to the silence sweep and
    /// flips the turn, so the two alternate step by step and neither starves
    /// the other (the catch-up seal is begun again every 5 s). O(1).
    pub(super) fn take_idle_turn(&mut self) -> bool {
        let silence = self.silence_turn;
        self.silence_turn = !silence;
        silence
    }

    /// Test and harness seam: slots per step (at least 1).
    #[cfg(test)]
    pub(super) fn set_step_slots(&mut self, slots: u32) {
        self.step_slots = slots.max(1);
    }
}

impl LiveIngest {
    /// Starts a silence sweep at `now_millis` (the 30 s timer arm). **O(1)**
    /// plus O(sockets) to open the chunked reconcile pass; no allocation.
    ///
    /// If the previous sweep is still pending (the drain had no idle time for
    /// a whole period), that stale sweep is dropped with its reconcile pass,
    /// counted on [`SILENCE_SWEEP_OVERRUN_COUNTER`], and the new sweep runs to
    /// its end at once on the fresh instant: O(tracked), the old one-go cost,
    /// and its result is returned for the page. An empty detector also
    /// finishes at once. So every timer period still yields exactly one
    /// judged sweep, on fresh data.
    #[must_use]
    pub fn begin_silence_sweep(&mut self, now_millis: u64) -> Option<SilenceDone> {
        let clock = (
            tickvault_core::websocket::silence_calendar::session_secs_of_day_now(),
            crate::subscribe_reconcile::ist_day_now(),
        );
        self.begin_silence_sweep_at(
            now_millis,
            Instant::now(),
            clock,
            now_ist_secs_of_day(),
            silence_page_allowed_today(),
        )
    }

    /// [`Self::begin_silence_sweep`] with every clock injected (tests and
    /// the harness).
    #[must_use]
    pub(super) fn begin_silence_sweep_at(
        &mut self,
        now_millis: u64,
        reconcile_now: Instant,
        reconcile_clock: (u32, i64),
        gate_secs: u64,
        page_allowed_today: bool,
    ) -> Option<SilenceDone> {
        let overran = self.drop_overrun_silence_sweep();
        let end = u32::try_from(self.tracked_instruments()).unwrap_or(u32::MAX);
        let reconcile = match self.subscribe_reconcile.as_ref() {
            // A pass left pending by an earlier owner (a drain that died
            // mid-sweep) is dropped, never finished: its partial counts
            // would plan rounds from half a sweep.
            Some(book) if !book.begin_chunked_at(reconcile_now) => {
                book.abandon_chunked();
                book.begin_chunked_at(reconcile_now)
            }
            Some(_) => true,
            None => false,
        };
        let mut sweep = SilenceSweep {
            now_millis,
            reconcile_clock,
            reconcile,
            gate_secs,
            page_allowed_today,
            end,
            cursor: 0,
            tally: SilenceTally::default(),
            worst: [SilentInstrument::EMPTY; WORST_SILENT_NAMED],
        };
        if overran || end == 0 {
            let _ = self.scan_silence_sweep_slots(&mut sweep, u32::MAX);
            return Some(self.finish_silence_sweep(&sweep));
        }
        self.silence_sweep.pending = Some(sweep);
        None
    }

    /// One idle step of the pending sweep: at most the step's slots, then,
    /// when it reaches the sweep's end, the finish. Returns the finished
    /// sweep. **O(step)** per call, one reconcile lock per step, no
    /// allocation; the call that reaches the end also runs the finish
    /// (the reconcile finish, O(sockets + candidates), or O(expected) on an
    /// IST day change; the two gauges; the dead-class verdict).
    #[must_use]
    pub fn step_silence_sweep(&mut self) -> Option<SilenceDone> {
        let mut sweep = self.silence_sweep.pending.take()?;
        if !self.scan_silence_sweep_slots(&mut sweep, self.silence_sweep.step_slots) {
            self.silence_sweep.pending = Some(sweep);
            return None;
        }
        Some(self.finish_silence_sweep(&sweep))
    }

    /// Drops a pending sweep with no page and no gauge (shutdown: the
    /// process is exiting). Its reconcile pass is dropped too. O(1).
    pub fn abandon_silence_sweep(&mut self) {
        if self.silence_sweep.pending.take().is_some()
            && let Some(book) = self.subscribe_reconcile.as_ref()
        {
            book.abandon_chunked();
        }
    }

    /// The overrun path: drops the stale pending sweep (its frozen instant is
    /// a whole period old) and its reconcile pass, counts it, and warns at
    /// powers of two. Returns whether one was pending. O(1).
    fn drop_overrun_silence_sweep(&mut self) -> bool {
        let Some(stale) = self.silence_sweep.pending.take() else {
            return false;
        };
        if stale.reconcile
            && let Some(book) = self.subscribe_reconcile.as_ref()
        {
            book.abandon_chunked();
        }
        let left = stale.end.saturating_sub(stale.cursor);
        let slot = &mut self.silence_sweep;
        slot.overruns = slot.overruns.saturating_add(1);
        slot.overrun_counter.increment(1);
        if slot.overruns.is_power_of_two() {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                counter = SILENCE_SWEEP_OVERRUN_COUNTER,
                overruns = slot.overruns,
                slots_left = left,
                "silence check: the 30 s timer fired while the previous sweep was still \
                 pending. That stale sweep was dropped and a fresh one ran at once, as before \
                 the split. Sustained growth means the drain has no idle time."
            );
        }
        true
    }

    /// Visits up to `count` slots of `sweep` from its cursor, noting each
    /// report in the reconcile chunk (one lock for the whole call) and the
    /// tally. Returns whether the sweep reached its end. O(count).
    fn scan_silence_sweep_slots(&self, sweep: &mut SilenceSweep, count: u32) -> bool {
        let mut chunk = if sweep.reconcile {
            self.subscribe_reconcile
                .as_ref()
                .and_then(|book| book.chunk())
        } else {
            None
        };
        let SilenceSweep {
            now_millis,
            end,
            cursor,
            tally,
            worst,
            ..
        } = sweep;
        let next = self.detector.scan_silence_slot_range(
            *now_millis,
            *cursor,
            *end,
            count.max(1),
            |slot, report| {
                if let Some(chunk) = chunk.as_mut() {
                    chunk.note_at_slot(slot, &report);
                }
                tally.observe(worst, &report);
            },
        );
        drop(chunk);
        // No progress means the detector holds fewer slots than `end` (only a
        // test's session reset shrinks it): the sweep is over.
        let progressed = next > *cursor;
        *cursor = next;
        next >= *end || !progressed
    }

    /// Ends a sweep: the reconcile finish at the frozen clock, then the
    /// gauges and the dead-class verdict ([`Self::finish_silence_tally`]).
    fn finish_silence_sweep(&self, sweep: &SilenceSweep) -> SilenceDone {
        if sweep.reconcile
            && let Some(book) = self.subscribe_reconcile.as_ref()
        {
            let _ = book.finish_chunked_at(sweep.reconcile_clock.0, sweep.reconcile_clock.1);
        }
        let (silent, never, named) =
            self.finish_silence_tally(&sweep.tally, sweep.now_millis, sweep.page_allowed_today);
        SilenceDone {
            silent,
            never,
            named,
            worst: sweep.worst,
            now_millis: sweep.now_millis,
            gate_secs: sweep.gate_secs,
            page_allowed_today: sweep.page_allowed_today,
            tracked: u64::from(sweep.end),
        }
    }

    /// The end of every silence scan, one-go or sliced: the two gauges and
    /// the dead-class verdict judged at `now_millis`. Returns
    /// `(silent, never, named)`. O(segment classes).
    pub(super) fn finish_silence_tally(
        &self,
        tally: &SilenceTally,
        now_millis: u64,
        page_allowed_today: bool,
    ) -> (u64, u64, usize) {
        let SilenceTally {
            silent,
            never,
            named,
            classes,
        } = *tally;
        metrics::gauge!(INSTRUMENTS_SILENT_GAUGE).set(silent as f64);
        metrics::gauge!(INSTRUMENTS_NEVER_TICKED_GAUGE).set(never as f64);
        // CORRECTED 2026-09-10: the class verdict is judged ONLY inside the
        // continuous session. Until today this line ran unconditionally, and
        // on 2026-09-10 it fired `RISK-GAP-03` for NSE_FNO at 08:33 IST and
        // again at 09:00 IST: ~22,000 option contracts were seeded at the 08:31
        // boot, their warmup elapsed, and — because NOTHING trades before the
        // 09:15 bell — every one of them was still never-ticked. That is
        // "dead" by construction, on every trading morning, and it drives the
        // `errcode-risk-gap-03` log-filter alarm (noise lock §2.3a). The
        // per-instrument silence PAGE on the 30 s arm has been session-gated
        // since 2026-08-12; this rollup sat above that gate, inside the scan,
        // and inherited none of it.
        //
        // Judged on `now_millis` — the same wall clock the scan itself uses —
        // rather than a second clock read, so a test can drive both halves
        // with one number. Outside the session the latch is CLEARED, not held:
        // the pre-open must not carry a verdict into the bell, and a post-close
        // latch would only ever be consulted by the next boot's fresh state.
        let scan_secs = ist_secs_of_day_from_millis(now_millis);
        if dead_class_verdict_is_due(is_within_market_hours_ist(scan_secs), page_allowed_today)
            || muhurat_silence_check_due_now(scan_secs)
        {
            self.report_dead_classes(&classes);
        } else {
            self.stand_down_dead_class_report();
        }
        (silent, never, named)
    }
}

/// What [`SilencePager::judge`] did with one finished sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PagerAction {
    /// Outside the session or not a trading day: latches cleared.
    OutOfSession,
    /// Nothing pending: the episode (if any) cleared.
    Cleared,
    /// Pending, but no line this sweep (confirming, latched or cooling).
    Counting,
    /// Below the paging floor: logged once at `warn!`.
    Logged,
    /// The RISK-GAP-03 page.
    Paged,
}

/// The silent-instrument page's latches, owned by the drain. Edge-triggered
/// per audit Rule 4: the rising edge fires once, the falling edge logs
/// recovery at info and re-arms.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SilencePager {
    /// Consecutive alarm-worthy sweeps.
    silent_scans: u32,
    /// Whether this episode already paged.
    silence_reported: bool,
    /// §2.19: an episode below the paging floor is logged once at `warn!`.
    /// A separate latch, so an episode that grows past the floor still pages.
    silence_logged: bool,
    /// Never-ticked instruments are paged once per session; see
    /// `silence_scan_pending` for why this is a separate latch.
    never_reported: bool,
    /// When the last RISK-GAP-03 page fired (unix seconds).
    last_silence_page: Option<u64>,
}

impl SilencePager {
    /// The page logic that ran at the end of the 30 s timer arm until
    /// 2026-11-24, unchanged in text, codes, fields and order, judged on the
    /// sweep's frozen values. O(named), no allocation.
    pub(super) fn judge(&mut self, done: &SilenceDone) -> PagerAction {
        let SilenceDone {
            silent,
            never,
            named,
            now_millis,
            ..
        } = *done;
        let worst_silent = &done.worst;
        // Gauges publish unconditionally — a dashboard reading zero
        // outside market hours is correct, and gating the gauge would
        // make "no data" and "nothing wrong" indistinguishable.
        // Only the PAGE is market-hours gated (audit Rule 3): the
        // whole universe is legitimately silent after 15:30, and a
        // detector that pages every evening gets muted by lunchtime.
        //
        // The TRADING-DAY half of that gate is just as load-bearing and
        // was missing until 2026-08-14. EventBridge starts this box on
        // `MON-FRI`, which includes NSE holidays, so on a weekday
        // holiday the lane seeds the whole universe, receives nothing —
        // correctly, the market is shut — and every seeded instrument
        // crosses the silence floor at once. The page that follows says
        // `silent=<universe>, never_ticked=<universe>`, which is
        // indistinguishable from a total subscribe failure. Every
        // sibling leg in this repo already gates on the calendar
        // (`groww_contract_1m_boot`, `groww_option_chain_1m_boot`,
        // `brutex_crossverify_boot`, `feed_scoreboard_boot`); the
        // revived lane was the one regression.
        //
        // US1-D (2026-11-24): both halves are the values frozen when the
        // sweep began (`gate_secs`, `page_allowed_today`).
        let silence_secs = done.gate_secs;
        // Audit L3: on a Muhurat day (an NSE holiday) the check runs
        // in the Muhurat continuous session too.
        if (!is_within_market_hours_ist(silence_secs) || !done.page_allowed_today)
            && !muhurat_silence_check_due_now(silence_secs)
        {
            self.silent_scans = 0;
            self.silence_reported = false;
            self.silence_logged = false;
            self.never_reported = false;
            return PagerAction::OutOfSession;
        }
        if !silence_scan_pending(silent, never, self.never_reported) {
            if self.silence_reported {
                info!(
                    silent,
                    never_ticked = never,
                    "Dhan live feed: every instrument that has ticked is ticking \
                     again within its own expected cadence"
                );
            }
            self.silent_scans = 0;
            // Re-arm whenever the went-quiet episode clears, even
            // while never-ticked instruments remain: those were
            // reported once (`never_reported`) and cannot change
            // except by ticking. Until 2026-10-06 this re-armed only
            // at `never == 0`, which a never-traded far strike keeps
            // false all day, so the page fired once per session
            // (see `silence_scan_pending`).
            //
            // This gate alone is NOT what stops the page storm; see
            // the cooldown at the emit below, and the measured numbers
            // recorded there.
            self.silence_reported = false;
            self.silence_logged = false;
            return PagerAction::Cleared;
        }
        self.silent_scans = self.silent_scans.saturating_add(1);
        let silent_scans = self.silent_scans;
        // A cooldown between PAGES, not between episodes.
        //
        // # What the 2026-08-14 session actually did
        //
        // 25 distinct RISK-GAP-03 emits in one trading day. The
        // `silent` count oscillated the whole time — 4, 9, 1, 2, 1, 3,
        // 208, 10 — clearing to zero between episodes and re-arming
        // the latch each time, entirely legitimately: these are
        // sparse-cadence instruments going quiet and coming back.
        //
        // So the per-episode latch was working exactly as designed and
        // still produced ~25 pages, because the real world produced ~25
        // episodes. `never_ticked` was 4 on the 09:15 emit and 0 on
        // every one after it, so the feed WAS delivering — gating on
        // never-ticked alone (above) would have suppressed almost none
        // of this.
        //
        // That became a paging problem on 2026-08-15, when RISK-GAP-03
        // gained a CloudWatch alarm: at a 5-minute window with a
        // recovery page, 25 episodes is ~50 operator messages a day.
        // Half an hour is chosen to sit above the observed inter-episode
        // gap (two to five minutes through the afternoon) while staying
        // far below the session, so a genuinely new problem hours later
        // still pages.
        //
        // The counter is deliberately NOT gated — every episode still
        // increments `tv_dhan_feed_instruments_silent`, so the
        // suppressed ones remain countable on the dashboard. Only the
        // page is rate-limited, and the log line says how many were
        // folded in.
        // `now_millis` is the sweep's frozen scan instant.
        let now_secs = now_millis / 1_000;
        let cooling =
            silence_page_is_cooling(self.last_silence_page, now_secs, SILENCE_PAGE_COOLDOWN_SECS);
        // §2.19 (2026-11-11): below the floor the episode is logged, not paged.
        // US1-D: the tracked count frozen when the sweep began (its `end`).
        let tracked_now = done.tracked;
        let pages = silence_warrants_page(silent, never, self.never_reported, tracked_now);
        let silence_reported = self.silence_reported;
        if silent_scans >= SILENCE_SCANS_BEFORE_ALERT
            && !silence_reported
            && !self.silence_logged
            && !pages
        {
            self.silence_logged = true;
            warn!(
                code = ErrorCode::RiskGapTickGap.code_str(),
                silent,
                never_ticked = never,
                tracked = tracked_now,
                page_floor = silence_page_floor(tracked_now),
                consecutive_scans = silent_scans,
                "Dhan live feed: {silent} subscribed instrument(s) are quiet beyond their \
                 own learned cadence, below the paging floor (1% of tracked); logged and \
                 counted, not paged"
            );
            for entry in worst_silent.iter().take(named) {
                warn!(
                    code = ErrorCode::RiskGapTickGap.code_str(),
                    security_id = entry.security_id,
                    segment = entry.segment.as_str(),
                    silent_millis = entry.silent_millis,
                    expected_millis = entry.expected_millis,
                    "quiet instrument named (below the paging floor)"
                );
            }
            return PagerAction::Logged;
        }
        if silent_scans >= SILENCE_SCANS_BEFORE_ALERT && !silence_reported && !cooling && pages {
            self.silence_reported = true;
            if never > 0 {
                self.never_reported = true;
            }
            self.last_silence_page = Some(now_secs);
            error!(
                code = ErrorCode::RiskGapTickGap.code_str(),
                silent,
                never_ticked = never,
                tracked = done.tracked,
                consecutive_scans = silent_scans,
                "Dhan live feed: {silent} subscribed instrument(s) are quiet beyond \
                 their own learned cadence across {silent_scans} consecutive 30s scans, \
                 of which {never} have produced NOTHING since being subscribed. A \
                 never-ticked instrument usually means its subscribe did not take — \
                 there is no other signal for that, because a stream that never \
                 arrives leaves nothing to count or fail to parse. \
                 Legitimately-sparse instruments are excluded from this count."
            );
            // NAME them. The line above says how many; these say WHICH.
            //
            // Until 2026-08-21 no `security_id` reached any log, metric
            // or table on this path, so "which instruments went silent
            // yesterday?" had no answer anywhere in the system — the
            // information was computed, counted, and thrown away. It is
            // also the one failure with no other evidence: a stream
            // that never arrives leaves nothing to count and nothing to
            // fail to parse, so absence against a seeded key is all
            // there is.
            //
            // One line per instrument rather than one line listing
            // them: each is greppable by `security_id`, each carries
            // its own cadence context, and no line needs a formatted
            // collection — so this stays allocation-free. Bounded by
            // `named` (<= WORST_SILENT_NAMED) and by the same
            // once-per-episode latch as the page above.
            for entry in worst_silent.iter().take(named) {
                error!(
                    code = ErrorCode::RiskGapTickGap.code_str(),
                    security_id = entry.security_id,
                    segment = entry.segment.as_str(),
                    silent_millis = entry.silent_millis,
                    expected_millis = entry.expected_millis,
                    never_ticked = entry.never_ticked,
                    "silent instrument named: quiet for {}ms against an earned cadence \
                     of {}ms{}",
                    entry.silent_millis,
                    entry.expected_millis,
                    if entry.never_ticked {
                        " — and it has produced NOTHING since being subscribed, which \
                         usually means the subscribe did not take"
                    } else {
                        ""
                    }
                );
            }
            return PagerAction::Paged;
        }
        PagerAction::Counting
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{PagerAction, SILENCE_PAGE_COOLDOWN_SECS, SilenceDone, SilencePager};
    use crate::subscribe_reconcile::SubscribeReconcile;
    use proptest::prelude::*;
    use std::time::Duration;
    use tickvault_core::pipeline::tick_gap_detector::TickObservation;
    use tickvault_core::websocket::pool_supervisor::LiveSubscriptionCommand;
    use tokio::sync::mpsc;

    const SEGMENTS: [ExchangeSegment; 3] = [
        ExchangeSegment::IdxI,
        ExchangeSegment::NseEquity,
        ExchangeSegment::NseFno,
    ];
    /// An IST day for the reconcile clock.
    const DAY: i64 = 20_000;
    /// 10:00 IST, inside the session and the reconcile window.
    const IN_SESSION: u32 = 10 * 3_600;
    /// A scan instant long after every seed below, so seeded instruments
    /// read silent and never-ticked.
    const LATE_MILLIS: u64 = 10_000_000_000;
    /// Unix millis on epoch day 0 whose IST time is inside 09:15 to 15:30
    /// (09:16:40 to 15:28:20 IST).
    const IN_SESSION_MILLIS: std::ops::Range<u64> = 13_600_000..35_900_000;

    fn lane(book: Option<&Arc<SubscribeReconcile>>, capacity: usize) -> LiveIngest {
        let ingest =
            LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8).with_detector_capacity(capacity);
        match book {
            Some(book) => ingest.with_subscribe_reconcile(Arc::clone(book)),
            None => ingest,
        }
    }

    /// Runs a sliced sweep to its end with injected clocks; returns the
    /// finished sweep and the idle steps it took.
    fn run_sliced(
        ingest: &mut LiveIngest,
        now: u64,
        at: Instant,
        clock: (u32, i64),
        gate_secs: u64,
        step: u32,
    ) -> (SilenceDone, u32) {
        ingest.silence_sweep.set_step_slots(step);
        if let Some(done) =
            ingest.begin_silence_sweep_at(now, at, clock, gate_secs, silence_page_allowed_today())
        {
            return (done, 0);
        }
        let mut steps = 0u32;
        loop {
            steps += 1;
            if let Some(done) = ingest.step_silence_sweep() {
                return (done, steps);
            }
        }
    }

    /// The one-go scan's result as a [`SilenceDone`] with the same frozen
    /// gate inputs, so the pager judges both alike.
    fn run_one_go(
        ingest: &LiveIngest,
        now: u64,
        at: Instant,
        clock: (u32, i64),
        gate_secs: u64,
    ) -> SilenceDone {
        let mut worst = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
        let (silent, never, named) = ingest.scan_silence_named_at(now, &mut worst, at, Some(clock));
        SilenceDone {
            silent,
            never,
            named,
            worst,
            now_millis: now,
            gate_secs,
            page_allowed_today: silence_page_allowed_today(),
            tracked: ingest.tracked_instruments() as u64,
        }
    }

    fn list(keys: &[(u64, usize)]) -> Vec<SubscribeInstrument> {
        keys.iter()
            .map(|(id, seg)| SubscribeInstrument {
                security_id: *id,
                segment: SEGMENTS[*seg],
            })
            .collect()
    }

    #[derive(Debug, Clone)]
    enum Op {
        Seed {
            id: u64,
            seg: usize,
            at: u64,
        },
        Tick {
            id: u64,
            seg: usize,
            ltt: u32,
            vol: u32,
            at: u64,
        },
        ResetSession,
        Record {
            socket: usize,
            keys: Vec<(u64, usize)>,
            at_ms: u64,
        },
        CloseSocket {
            socket: usize,
        },
        Forget {
            keys: Vec<(u64, usize)>,
        },
        Sweep {
            now: u64,
            at_ms: u64,
            secs: u32,
            day: i64,
            gate: u64,
            step: u32,
        },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let keys = || prop::collection::vec((0u64..12, 0usize..3), 0..6);
        prop_oneof![
            4 => (0u64..12, 0usize..3, 0u64..2_000_000)
                .prop_map(|(id, seg, at)| Op::Seed { id, seg, at }),
            6 => (0u64..12, 0usize..3, 0u32..60, 0u32..5_000, 0u64..2_000_000)
                .prop_map(|(id, seg, ltt, vol, at)| Op::Tick { id, seg, ltt, vol, at }),
            1 => Just(Op::ResetSession),
            3 => (0usize..3, keys(), 0u64..900_000)
                .prop_map(|(socket, keys, at_ms)| Op::Record { socket, keys, at_ms }),
            1 => (0usize..3).prop_map(|socket| Op::CloseSocket { socket }),
            1 => keys().prop_map(|keys| Op::Forget { keys }),
            5 => (
                // A third of the sweeps fall inside 09:15 to 15:30 IST on
                // epoch day 0, so the dead-class report (its grace counter
                // and latch) runs, not only its stand-down.
                prop_oneof![2 => 0u64..2_500_000, 1 => IN_SESSION_MILLIS],
                0u64..900_000,
                prop_oneof![3 => 33_420u32..54_900, 1 => 0u32..86_400],
                0i64..3,
                prop_oneof![3 => 33_300u64..55_800, 1 => 0u64..86_400],
                1u32..18,
            )
                .prop_map(|(now, at_ms, secs, day, gate, step)| Op::Sweep {
                    now,
                    at_ms,
                    secs,
                    day: DAY + day,
                    gate,
                    step,
                }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// A sweep run in idle steps of random size, with nothing touching
        /// the detector or the book between steps, equals the one-go scan at
        /// the same instant: counts, named instruments, the dead-class latch,
        /// every reconcile book field, every round, and the pager's action
        /// and latches.
        #[test]
        fn proptest_sliced_silence_sweep_matches_the_one_go_scan(
            auto in any::<bool>(),
            ops in prop::collection::vec(op_strategy(), 1..80),
        ) {
            let (book_new, mut rounds_new) = SubscribeReconcile::new(auto);
            let (book_old, mut rounds_old) = SubscribeReconcile::new(auto);
            let mut new = lane(Some(&book_new), 16);
            let mut old = lane(Some(&book_old), 16);
            let mut pager_new = SilencePager::default();
            let mut pager_old = SilencePager::default();
            let base = Instant::now();
            let at = |ms: u64| base + Duration::from_millis(ms);
            let mut sockets: Vec<(
                mpsc::Sender<LiveSubscriptionCommand>,
                Option<mpsc::Receiver<LiveSubscriptionCommand>>,
            )> = (0..3)
                .map(|_| {
                    let (tx, rx) = mpsc::channel(4);
                    (tx, Some(rx))
                })
                .collect();
            for op in ops {
                match op {
                    Op::Seed { id, seg, at } => {
                        let s = SEGMENTS[seg];
                        prop_assert_eq!(new.seed(id, s, at), old.seed(id, s, at));
                    }
                    Op::Tick { id, seg, ltt, vol, at } => {
                        let obs = TickObservation {
                            key: (id, SEGMENTS[seg]),
                            ltt_epoch_secs: 1_786_800_000 + ltt,
                            volume: vol,
                            open_interest: 0,
                            recv_monotonic_millis: at,
                        };
                        prop_assert_eq!(new.detector.observe(obs), old.detector.observe(obs));
                    }
                    Op::ResetSession => {
                        new.detector.reset_session();
                        old.detector.reset_session();
                    }
                    Op::Record { socket, keys, at_ms } => {
                        let l = list(&keys);
                        book_new.record_subscribed_at(&sockets[socket].0, &l, at(at_ms));
                        book_old.record_subscribed_at(&sockets[socket].0, &l, at(at_ms));
                    }
                    Op::CloseSocket { socket } => {
                        sockets[socket].1 = None;
                        let (tx, rx) = mpsc::channel(4);
                        sockets[socket] = (tx, Some(rx));
                    }
                    Op::Forget { keys } => {
                        let l = list(&keys);
                        book_new.forget(&l);
                        book_old.forget(&l);
                    }
                    Op::Sweep { now, at_ms, secs, day, gate, step } => {
                        let clock = (secs, day);
                        let tracked = new.tracked_instruments() as u32;
                        let (got, steps) = run_sliced(&mut new, now, at(at_ms), clock, gate, step);
                        let want = run_one_go(&old, now, at(at_ms), clock, gate);
                        prop_assert_eq!(got, want);
                        prop_assert!(!new.idle_work_pending(), "the sweep finished");
                        prop_assert!(steps <= tracked.div_ceil(step).max(1));
                        prop_assert_eq!(
                            new.dead_class_latch.load(Ordering::Relaxed),
                            old.dead_class_latch.load(Ordering::Relaxed)
                        );
                        for (a, b) in new
                            .dead_class_dead_scans
                            .iter()
                            .zip(old.dead_class_dead_scans.iter())
                        {
                            prop_assert_eq!(a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
                        }
                        prop_assert_eq!(pager_new.judge(&got), pager_old.judge(&want));
                        prop_assert_eq!(&pager_new, &pager_old);
                    }
                }
                book_new.assert_cell_invariant();
                prop_assert_eq!(book_new.test_fingerprint(), book_old.test_fingerprint());
                loop {
                    match (rounds_new.try_recv(), rounds_old.try_recv()) {
                        (Ok(a), Ok(b)) => prop_assert_eq!(a.test_view(), b.test_view()),
                        (Err(_), Err(_)) => break,
                        (a, b) => prop_assert!(
                            false,
                            "rounds differ: {} vs {}",
                            a.is_ok(),
                            b.is_ok()
                        ),
                    }
                }
            }
        }
    }

    fn seeded(count: u64) -> LiveIngest {
        let mut ingest = lane(None, 16);
        for id in 0..count {
            assert!(ingest.seed(100 + id, ExchangeSegment::NseFno, 0));
        }
        ingest
    }

    #[test]
    fn test_begin_silence_sweep_on_an_empty_detector_finishes_at_once() {
        let mut ingest = lane(None, 16);
        let done = ingest
            .begin_silence_sweep(LATE_MILLIS)
            .expect("an empty detector has nothing to step through");
        assert_eq!(
            (done.silent, done.never, done.named, done.tracked),
            (0, 0, 0, 0)
        );
        assert_eq!(done.now_millis, LATE_MILLIS);
        assert!(!ingest.idle_work_pending());
        assert_eq!(ingest.step_idle_work(), IdleStep::default());
    }

    #[test]
    fn test_step_silence_sweep_returns_done_only_at_the_end() {
        let mut ingest = seeded(5);
        assert!(ingest.step_silence_sweep().is_none(), "nothing pending");
        ingest.silence_sweep.set_step_slots(2);
        let begun = ingest.begin_silence_sweep_at(
            LATE_MILLIS,
            Instant::now(),
            (IN_SESSION, DAY),
            u64::from(IN_SESSION),
            true,
        );
        assert!(begun.is_none());
        assert!(ingest.idle_work_pending());
        assert!(ingest.step_silence_sweep().is_none(), "slots 0..2");
        assert!(ingest.step_silence_sweep().is_none(), "slots 2..4");
        let done = ingest.step_silence_sweep().expect("slot 4 ends the sweep");
        assert_eq!((done.silent, done.never, done.tracked), (5, 5, 5));
        assert_eq!(done.named, 5);
        assert!(!ingest.idle_work_pending());
        assert!(ingest.step_silence_sweep().is_none());
    }

    /// The idle arm steps the sweep after the seed and catch-up work, and
    /// hands the finished sweep out in `silence_done`.
    #[test]
    fn test_step_idle_work_steps_the_silence_sweep_last() {
        let mut ingest = seeded(3);
        ingest.silence_sweep.set_step_slots(2);
        let _ = ingest.begin_silence_sweep(LATE_MILLIS);
        let queued = ingest.queue_seed_batch(
            vec![SubscribeInstrument {
                security_id: 900,
                segment: ExchangeSegment::NseFno,
            }],
            1,
        );
        assert!(queued.is_none(), "the queue has room");
        let first = ingest.step_idle_work();
        assert!(first.seed_done.is_some(), "the seed batch goes first");
        assert!(first.silence_done.is_none());
        assert!(ingest.step_idle_work().silence_done.is_none());
        let last = ingest.step_idle_work();
        let done = last.silence_done.expect("the second silence step ends it");
        assert_eq!(done.tracked, 3, "the slot seeded mid-sweep waits");
        assert!(!ingest.idle_work_pending());
    }

    #[test]
    fn test_timer_overrun_drops_the_stale_sweep_and_scans_fresh_inline() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let mut ingest = lane(Some(&book), 16);
        let mut twin = lane(None, 16);
        for id in 0..6 {
            assert!(ingest.seed(100 + id, ExchangeSegment::NseFno, 0));
            assert!(twin.seed(100 + id, ExchangeSegment::NseFno, 0));
        }
        ingest.silence_sweep.set_step_slots(2);
        let clock = (IN_SESSION, DAY);
        let gate = u64::from(IN_SESSION);
        let at = Instant::now();
        assert!(
            ingest
                .begin_silence_sweep_at(LATE_MILLIS, at, clock, gate, true)
                .is_none()
        );
        assert!(ingest.step_silence_sweep().is_none());
        // A slot added after the stale sweep began: the fresh sweep sees it.
        assert!(ingest.seed(700, ExchangeSegment::NseFno, 0));
        assert!(twin.seed(700, ExchangeSegment::NseFno, 0));
        let later = LATE_MILLIS + 30_000;
        let fresh = ingest
            .begin_silence_sweep_at(later, at, clock, gate, true)
            .expect("the overrun runs the fresh sweep at once");
        assert_eq!(ingest.silence_sweep.overruns, 1);
        let want = run_one_go(&twin, later, at, clock, gate);
        assert_eq!(
            (
                fresh.silent,
                fresh.never,
                fresh.named,
                fresh.worst,
                fresh.tracked
            ),
            (
                want.silent,
                want.never,
                want.named,
                want.worst,
                want.tracked
            ),
            "judged at the fresh instant on the current slots, not the stale sweep's"
        );
        assert_eq!((fresh.now_millis, fresh.tracked), (later, 7));
        assert!(!ingest.idle_work_pending(), "nothing is left pending");
        assert!(
            book.chunk().is_none(),
            "the stale reconcile pass was dropped and the fresh one finished"
        );
        // The next timer, with idle steps meanwhile, is no overrun.
        assert!(
            ingest
                .begin_silence_sweep_at(later + 30_000, at, clock, gate, true)
                .is_none()
        );
        let mut done = None;
        while done.is_none() {
            done = ingest.step_silence_sweep();
        }
        assert_eq!(done.map(|d| d.now_millis), Some(later + 30_000));
        assert_eq!(
            ingest.silence_sweep.overruns, 1,
            "a sweep that finished in steps is no overrun"
        );
    }

    /// The equivalence proptest runs a third of its sweeps inside the
    /// session; this pins that the sliced finish takes the in-session
    /// dead-class report there (grace counter, then latch), and that it
    /// matches the one-go scan when today's calendar allows the page.
    #[test]
    fn test_sliced_sweep_in_session_runs_the_dead_class_report() {
        let now = IN_SESSION_MILLIS.start + 3_600_000;
        assert!(
            is_within_market_hours_ist(ist_secs_of_day_from_millis(now)),
            "the fixture instant is in session"
        );
        let mut sliced = lane(None, 16);
        let mut one_go = lane(None, 16);
        for id in 0..4 {
            assert!(sliced.seed(100 + id, ExchangeSegment::NseFno, 0));
            assert!(one_go.seed(100 + id, ExchangeSegment::NseFno, 0));
        }
        sliced.silence_sweep.set_step_slots(1);
        let gate = ist_secs_of_day_from_millis(now);
        let clock = (u32::try_from(gate).unwrap_or(0), DAY);
        let at = Instant::now();
        let allowed = silence_page_allowed_today();
        for scan in 0..DEAD_CLASS_SCANS_BEFORE_REPORT {
            // An explicit trading day for the sliced lane: the report must run.
            let mut done = sliced.begin_silence_sweep_at(now, at, clock, gate, true);
            while done.is_none() {
                done = sliced.step_silence_sweep();
            }
            let _ = run_one_go(&one_go, now, at, clock, gate);
            if scan + 1 < DEAD_CLASS_SCANS_BEFORE_REPORT {
                assert_eq!(sliced.dead_class_latch.load(Ordering::Relaxed), 0, "grace");
            }
        }
        assert_ne!(
            sliced.dead_class_latch.load(Ordering::Relaxed),
            0,
            "the in-session report latched the dead class"
        );
        if allowed {
            assert_eq!(
                sliced.dead_class_latch.load(Ordering::Relaxed),
                one_go.dead_class_latch.load(Ordering::Relaxed)
            );
        }
    }

    #[test]
    fn test_slots_added_mid_sweep_wait_for_the_next_sweep() {
        let mut ingest = seeded(3);
        ingest.silence_sweep.set_step_slots(1);
        let gate = u64::from(IN_SESSION);
        assert!(
            ingest
                .begin_silence_sweep_at(LATE_MILLIS, Instant::now(), (IN_SESSION, DAY), gate, true)
                .is_none()
        );
        assert!(ingest.step_silence_sweep().is_none());
        assert!(ingest.seed(700, ExchangeSegment::NseFno, 0));
        assert!(ingest.seed(701, ExchangeSegment::NseFno, 0));
        let mut done = None;
        while done.is_none() {
            done = ingest.step_silence_sweep();
        }
        let done = done.expect("finished");
        assert_eq!((done.tracked, done.silent), (3, 3));
        assert!(done.worst[..done.named].iter().all(|w| w.security_id < 700));
        let mut next = ingest.begin_silence_sweep_at(
            LATE_MILLIS,
            Instant::now(),
            (IN_SESSION, DAY),
            gate,
            true,
        );
        while next.is_none() {
            next = ingest.step_silence_sweep();
        }
        let next = next.expect("finished");
        assert_eq!(
            (next.tracked, next.silent),
            (5, 5),
            "the next sweep sees them"
        );
    }

    #[test]
    fn test_abandon_silence_sweep_leaves_nothing_pending() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let mut ingest = lane(Some(&book), 16);
        for id in 0..4 {
            assert!(ingest.seed(100 + id, ExchangeSegment::NseFno, 0));
        }
        ingest.silence_sweep.set_step_slots(1);
        assert!(ingest.begin_silence_sweep(LATE_MILLIS).is_none());
        assert!(ingest.step_silence_sweep().is_none());
        assert!(book.chunk().is_some(), "the reconcile pass is pending");
        ingest.abandon_silence_sweep();
        assert!(!ingest.idle_work_pending());
        assert!(ingest.step_silence_sweep().is_none());
        assert!(book.chunk().is_none(), "the reconcile pass is dropped too");
        ingest.abandon_silence_sweep();
        // A new sweep begins cleanly afterwards.
        assert!(ingest.begin_silence_sweep(LATE_MILLIS).is_none());
        assert!(book.chunk().is_some());
    }

    /// A pass left pending in the book by an earlier owner is dropped, not
    /// finished, and the new sweep takes its place.
    #[test]
    fn test_begin_silence_sweep_drops_a_stale_reconcile_pass() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let mut ingest = lane(Some(&book), 16);
        assert!(ingest.seed(100, ExchangeSegment::NseFno, 0));
        assert!(book.begin_chunked_at(Instant::now()));
        ingest.silence_sweep.set_step_slots(1);
        assert!(ingest.begin_silence_sweep(LATE_MILLIS).is_none());
        let done = ingest.step_silence_sweep().expect("one slot, one step");
        assert_eq!(done.tracked, 1);
        assert!(book.chunk().is_none(), "the pass was finished");
    }

    fn done_with(silent: u64, never: u64, gate_secs: u64, now_millis: u64) -> SilenceDone {
        let mut worst = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
        worst[0] = SilentInstrument {
            security_id: 42,
            segment: ExchangeSegment::NseFno,
            silent_millis: 600_000,
            expected_millis: 60_000,
            never_ticked: never > 0,
        };
        SilenceDone {
            silent,
            never,
            named: 1,
            worst,
            now_millis,
            gate_secs,
            page_allowed_today: true,
            tracked: 1_000,
        }
    }

    #[test]
    fn test_silence_pager_judge_pages_on_the_second_scan_then_cools() {
        let gate = u64::from(IN_SESSION);
        let mut pager = SilencePager::default();
        let now = LATE_MILLIS;
        assert_eq!(
            pager.judge(&done_with(50, 0, gate, now)),
            PagerAction::Counting
        );
        assert_eq!(
            pager.judge(&done_with(50, 0, gate, now)),
            PagerAction::Paged
        );
        assert_eq!(
            pager.judge(&done_with(50, 0, gate, now)),
            PagerAction::Counting
        );
        assert_eq!(
            pager.judge(&done_with(0, 0, gate, now)),
            PagerAction::Cleared
        );
        // A new episode inside the 30 minutes does not page again.
        assert_eq!(
            pager.judge(&done_with(50, 0, gate, now + 60_000)),
            PagerAction::Counting
        );
        assert_eq!(
            pager.judge(&done_with(50, 0, gate, now + 90_000)),
            PagerAction::Counting
        );
        // Past the cooldown it does.
        let later = now + (SILENCE_PAGE_COOLDOWN_SECS + 1) * 1_000;
        assert_eq!(
            pager.judge(&done_with(50, 0, gate, later)),
            PagerAction::Paged
        );
    }

    #[test]
    fn test_silence_pager_judge_logs_below_the_floor_and_clears_out_of_session() {
        let gate = u64::from(IN_SESSION);
        let mut pager = SilencePager::default();
        assert_eq!(
            pager.judge(&done_with(2, 0, gate, LATE_MILLIS)),
            PagerAction::Counting
        );
        assert_eq!(
            pager.judge(&done_with(2, 0, gate, LATE_MILLIS)),
            PagerAction::Logged
        );
        assert_eq!(
            pager.judge(&done_with(2, 0, gate, LATE_MILLIS)),
            PagerAction::Counting
        );
        // 03:00 IST: out of session, every latch cleared.
        assert_eq!(
            pager.judge(&done_with(2, 0, 3 * 3_600, LATE_MILLIS)),
            PagerAction::OutOfSession
        );
        assert_eq!(pager, SilencePager::default());
        let mut holiday = done_with(50, 0, gate, LATE_MILLIS);
        holiday.page_allowed_today = false;
        // Not a trading day: no page whatever the count (Muhurat aside).
        if !muhurat_silence_check_due_now(gate) {
            assert_eq!(pager.judge(&holiday), PagerAction::OutOfSession);
        }
    }
}
