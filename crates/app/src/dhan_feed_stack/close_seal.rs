//! The day's last candle bars are sealed at the close, not at shutdown (plan
//! item PR23, 2026-11-23).
//!
//! The catch-up seal's cutoff is `min(watermark, wall) − margin`. The
//! watermark is the newest vendor trade stamp, and it stops moving with the
//! last trade of the session, so the cutoff stops about 240 s before the
//! close. The day's last bars (every bucket ending after about 15:36, and the
//! last 15, 30 and 60 minute bars) then waited for the shutdown seal, and a
//! crash before shutdown left them to the next boot's WAL replay.
//!
//! From the session end plus the lateness margin plus one sweep interval
//! (15:44:05 IST on a regular day), [`close_seal_cutoff`] gives a second
//! cutoff: the end plus [`LONGEST_BUCKET_SECS`], capped at the next session's
//! start that day and at midnight. The catch-up takes the larger of the two,
//! so the same sliced sweep (O(1) to start, 256 slots a step on the drain's
//! idle arm) seals every bar of the session. Nothing is sealed earlier than a
//! late trade (up to the margin) could still reach it: the fold refuses
//! trades stamped at or after the end, and the term starts only once the
//! margin has passed.
//!
//! The sessions of one IST day are kept in a [`ClosePlan`], rebuilt once a
//! day (one calendar check, one Muhurat probe). The regular `[09:00, 15:40)`
//! session is in every day's plan, trading day or not, because the fold
//! admits trades there by time of day alone (a weekend daytime Muhurat or a
//! mock trading Saturday folds them too). A Muhurat evening session is the
//! fold's own evening window ([`super::mid_session_exit::muhurat_exit_window`]).
//!
//! # Honest limits
//! - The close term reads the box clock alone, where the usual cutoff is
//!   capped by `min(watermark, wall)`, and it starts 5 s after the lateness
//!   margin (245 − 240). A box clock more than about 5 s ahead of exchange
//!   time at the close therefore seals the day's last buckets before a trade
//!   stamped just before the end and delivered at the full 240 s margin; that
//!   trade then lands as a late amendment of the sealed bar, as any trade
//!   past the margin does. The clock-offset page (`CLOCK-SKEW-01`, over 1 ms
//!   for 5 minutes) watches the box clock, so this needs an unpaged clock
//!   error 5,000 times that threshold.
//! - The drain's wall reading comes from two clocks that are not ordered (a
//!   frame's receipt stamp and the catch-up timer's `now`), so within about a
//!   second of midnight the IST day can read back and forth and the plan is
//!   rebuilt more than once (each rebuild is cheap and allocation-free; the
//!   no-calendar `warn!` can then repeat, never in production, where the
//!   calendar is installed before the lane starts).
//!
//! # Complexity
//! [`close_seal_cutoff`] is O(sessions), at most 2, no allocation.

use tickvault_common::constants::{
    SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_trading::candles::{TF_COUNT, TfIndex};

use super::{CATCHUP_LATENESS_MARGIN_SECS, CATCHUP_SEAL_INTERVAL_SECS};

/// The longest candle bucket the fold carries, in seconds (3,600 today).
pub const LONGEST_BUCKET_SECS: u32 = longest_bucket_secs();

const fn longest_bucket_secs() -> u32 {
    let mut longest = 0_u32;
    let mut i = 0;
    while i < TF_COUNT {
        let secs = TfIndex::ALL[i].seconds_per_bucket();
        if secs > longest {
            longest = secs;
        }
        i += 1;
    }
    longest
}

const _: () = {
    let mut i = 0;
    while i < TF_COUNT {
        assert!(
            TfIndex::ALL[i].seconds_per_bucket() <= LONGEST_BUCKET_SECS,
            "LONGEST_BUCKET_SECS must cover every timeframe"
        );
        i += 1;
    }
};

/// How long after a session end the close term starts: the lateness margin
/// a quiet bucket waits for, plus one catch-up sweep interval (245 s).
pub const CLOSE_SEAL_DELAY_SECS: u32 =
    CATCHUP_LATENESS_MARGIN_SECS + CATCHUP_SEAL_INTERVAL_SECS as u32;

/// Sessions one IST day can hold: the regular one and a Muhurat evening.
const MAX_SESSIONS: usize = 2;

/// The close-seal cutoff, or 0 when no session qualifies.
///
/// `wall` and `watermark` are IST epoch seconds; `sessions` are `[start,
/// end)` IST epoch seconds of one day (end exclusive); `day_end` is that
/// day's next midnight. A session qualifies when `end + CLOSE_SEAL_DELAY_SECS
/// <= wall` and `watermark >= start` (this process folded a trade of it, or
/// later). The LATEST qualifying session gives `min(end +
/// LONGEST_BUCKET_SECS, next session start that day, day_end)`.
///
/// # Complexity
/// O(sessions), pure, no allocation.
#[must_use]
pub fn close_seal_cutoff(wall: u32, watermark: u32, sessions: &[(u32, u32)], day_end: u32) -> u32 {
    let mut chosen: Option<(u32, u32)> = None;
    for &(start, end) in sessions {
        let due = end.saturating_add(CLOSE_SEAL_DELAY_SECS) <= wall;
        if due && watermark >= start && chosen.is_none_or(|(s, _)| start >= s) {
            chosen = Some((start, end));
        }
    }
    let Some((start, end)) = chosen else {
        return 0;
    };
    let mut cutoff = end.saturating_add(LONGEST_BUCKET_SECS).min(day_end);
    for &(next_start, _) in sessions {
        if next_start > start {
            cutoff = cutoff.min(next_start);
        }
    }
    cutoff
}

/// The sessions of one IST day, for [`close_seal_cutoff`]. `Copy`, fixed size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosePlan {
    /// The IST day (IST epoch seconds / 86,400) the plan is for; `i64::MIN`
    /// when unset.
    pub ist_day: i64,
    sessions: [(u32, u32); MAX_SESSIONS],
    len: u8,
    day_end: u32,
}

impl ClosePlan {
    /// No plan: every cutoff is 0 and the next wall reading rebuilds it.
    pub const UNSET: Self = Self {
        ist_day: i64::MIN,
        sessions: [(0, 0); MAX_SESSIONS],
        len: 0,
        day_end: 0,
    };

    /// The plan for IST day `ist_day`: the regular `[09:00, 15:40)` session
    /// when `regular`, plus `muhurat` (`[start, end)` IST seconds of day) when
    /// given, sorted by start. A day that does not fit in `u32` seconds gets
    /// no sessions. Pure, O(1).
    #[must_use]
    pub fn build(ist_day: i64, regular: bool, muhurat: Option<(u32, u32)>) -> Self {
        let mut plan = Self {
            ist_day,
            ..Self::UNSET
        };
        let Some(day_start) = ist_day
            .checked_mul(i64::from(SECONDS_PER_DAY))
            .and_then(|s| u32::try_from(s).ok())
        else {
            return plan;
        };
        plan.day_end = day_start.saturating_add(SECONDS_PER_DAY);
        let mut push = |(start, end): (u32, u32)| {
            if let Some(slot) = plan.sessions.get_mut(usize::from(plan.len)) {
                *slot = (
                    day_start.saturating_add(start),
                    day_start.saturating_add(end),
                );
                plan.len = plan.len.saturating_add(1);
            }
        };
        if regular {
            push((
                TICK_PERSIST_START_SECS_OF_DAY_IST,
                TICK_PERSIST_END_SECS_OF_DAY_IST,
            ));
        }
        if let Some(window) = muhurat {
            push(window);
        }
        if let [a, b] = &mut plan.sessions
            && plan.len == 2
            && b.0 < a.0
        {
            std::mem::swap(a, b);
        }
        plan
    }

    /// The plan for IST day `ist_day` from the installed trading calendar:
    /// the regular `[09:00, 15:40)` session on EVERY day, plus the date's
    /// Muhurat evening window. With no calendar installed, the regular
    /// session only, and `true` in the second slot (the caller logs it).
    ///
    /// The regular session is in every day's plan, trading day or not,
    /// because the fold admits trades in `[09:00, 15:40)` by time of day
    /// alone, whatever the calendar says: a daytime Muhurat on a weekend (the
    /// shipped 2026 shape, Sunday 2026-11-08, 14:05 to 15:40) or an NSE mock
    /// trading Saturday folds trades there, and their last bars need the same
    /// close seal (PR23 review, 2026-11-22: gating it on the trading-day flag
    /// left those days to the shutdown seal). A day with no trade costs
    /// nothing: the term needs this process to have folded a trade of the
    /// session (the watermark at or past its start).
    ///
    /// O(1): one calendar check and at most two hash probes.
    #[must_use]
    pub fn for_installed_calendar(ist_day: i64) -> (Self, bool) {
        let day_start = ist_day.saturating_mul(i64::from(SECONDS_PER_DAY));
        let date = chrono::DateTime::from_timestamp(day_start, 0).map(|t| t.date_naive());
        let info = date.and_then(tickvault_common::market_hours::installed_calendar_session_day);
        match info {
            Some(info) => {
                let muhurat = if info.muhurat_end_secs.is_some() {
                    super::mid_session_exit::muhurat_exit_window(day_start)
                } else {
                    None
                };
                (Self::build(ist_day, true, muhurat), false)
            }
            None => (Self::build(ist_day, true, None), true),
        }
    }

    /// The plan's sessions, `[start, end)` IST epoch seconds. O(1).
    #[must_use]
    pub fn sessions(&self) -> &[(u32, u32)] {
        self.sessions.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// [`close_seal_cutoff`] for this plan, or 0 when `wall` is not on the
    /// plan's day. O(sessions).
    #[must_use]
    pub fn cutoff(&self, wall: u32, watermark: u32) -> u32 {
        if i64::from(wall / SECONDS_PER_DAY) != self.ist_day {
            return 0;
        }
        close_seal_cutoff(wall, watermark, self.sessions(), self.day_end)
    }
}

#[cfg(test)]
mod close_seal_cutoff_tests {
    use super::*;
    use proptest::prelude::*;
    use tickvault_common::constants::{
        MUHURAT_PERSIST_END_SECS_OF_DAY_IST, MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
    };

    /// An IST day (IST epoch seconds / 86,400).
    const DAY: i64 = 20_594;
    const DAY_START: u32 = 20_594 * 86_400;

    const fn at(h: u32, m: u32, s: u32) -> u32 {
        DAY_START + h * 3_600 + m * 60 + s
    }

    fn regular() -> ClosePlan {
        ClosePlan::build(DAY, true, None)
    }

    #[test]
    fn test_longest_bucket_secs_is_the_hour() {
        assert_eq!(LONGEST_BUCKET_SECS, 3_600);
        assert_eq!(CLOSE_SEAL_DELAY_SECS, 245);
    }

    #[test]
    fn test_close_seal_cutoff_zero_until_end_plus_margin_plus_interval() {
        let plan = regular();
        assert_eq!(plan.cutoff(at(15, 44, 4), at(15, 39, 50)), 0);
        assert_eq!(plan.cutoff(at(15, 44, 5), at(15, 39, 50)), at(16, 40, 0));
        assert_eq!(plan.cutoff(at(16, 20, 0), at(15, 39, 50)), at(16, 40, 0));
    }

    #[test]
    fn test_close_seal_cutoff_zero_without_session_trade() {
        let plan = regular();
        // No trade folded today (yesterday's watermark, or an empty boot).
        assert_eq!(plan.cutoff(at(15, 50, 0), 0), 0);
        assert_eq!(plan.cutoff(at(15, 50, 0), DAY_START - 600), 0);
        assert_eq!(plan.cutoff(at(15, 50, 0), at(8, 59, 59)), 0);
        assert_eq!(plan.cutoff(at(15, 50, 0), at(9, 0, 0)), at(16, 40, 0));
    }

    #[test]
    fn test_close_seal_cutoff_capped_at_next_session_start() {
        let plan = ClosePlan::build(
            DAY,
            true,
            Some((
                MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
                MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
            )),
        );
        // A session starting at 16:00 would cut the regular term there.
        let early = close_seal_cutoff(
            at(15, 44, 5),
            at(15, 39, 0),
            &[(at(9, 0, 0), at(15, 40, 0)), (at(16, 0, 0), at(17, 0, 0))],
            DAY_START + 86_400,
        );
        assert_eq!(early, at(16, 0, 0));
        // The fold's evening window (18:00) is past 16:40: no cap needed.
        assert_eq!(plan.cutoff(at(15, 44, 5), at(15, 39, 0)), at(16, 40, 0));
        // Before the evening end + delay, an evening trade leaves the regular
        // term in place.
        assert_eq!(plan.cutoff(at(19, 0, 0), at(18, 30, 0)), at(16, 40, 0));
    }

    #[test]
    fn test_close_seal_cutoff_capped_at_midnight() {
        let day_end = DAY_START + 86_400;
        let cutoff = close_seal_cutoff(
            at(23, 55, 0),
            at(23, 0, 0),
            &[(at(22, 0, 0), at(23, 30, 0))],
            day_end,
        );
        assert_eq!(cutoff, day_end);
    }

    #[test]
    fn test_close_seal_cutoff_muhurat_published_end() {
        let plan = ClosePlan::build(
            DAY,
            true,
            Some((
                MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
                MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
            )),
        );
        assert_eq!(plan.cutoff(at(19, 34, 4), at(19, 29, 0)), at(16, 40, 0));
        assert_eq!(plan.cutoff(at(19, 34, 5), at(19, 29, 0)), at(20, 30, 0));
    }

    #[test]
    fn test_close_seal_cutoff_muhurat_only_date_no_regular_term() {
        let plan = ClosePlan::build(
            DAY,
            false,
            Some((
                MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
                MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
            )),
        );
        assert_eq!(plan.sessions().len(), 1);
        assert_eq!(plan.cutoff(at(15, 44, 5), at(15, 39, 0)), 0);
        assert_eq!(plan.cutoff(at(19, 34, 5), at(19, 0, 0)), at(20, 30, 0));
    }

    #[test]
    fn test_build_sorts_sessions_and_cutoff_checks_the_day() {
        let plan = ClosePlan::build(DAY, true, Some((7 * 3_600, 8 * 3_600)));
        assert_eq!(
            plan.sessions(),
            &[(at(7, 0, 0), at(8, 0, 0)), (at(9, 0, 0), at(15, 40, 0))]
        );
        // A wall on another day reads 0.
        assert_eq!(plan.cutoff(at(15, 50, 0) + 86_400, at(15, 39, 0)), 0);
        assert_eq!(ClosePlan::UNSET.cutoff(at(15, 50, 0), at(15, 39, 0)), 0);
        assert!(ClosePlan::UNSET.sessions().is_empty());
        // A day past u32 seconds gets no sessions.
        assert!(ClosePlan::build(i64::MAX, true, None).sessions().is_empty());
    }

    #[test]
    fn test_for_installed_calendar_keeps_the_day() {
        // A unit-test process may or may not have a calendar installed; either
        // way a weekday plan holds at most two sessions and the day is kept.
        let (plan, _missing) = ClosePlan::for_installed_calendar(DAY);
        assert_eq!(plan.ist_day, DAY);
        assert!(plan.sessions().len() <= 2);
    }

    proptest! {
        /// No bucket a close-seal cutoff seals can hold a tick of a session
        /// starting after the one that qualified, and nothing is sealed before
        /// that session's end plus the delay.
        #[test]
        fn proptest_close_seal_cutoff_never_seals_a_bucket_holding_a_later_session_tick(
            reg in any::<bool>(),
            m_start in TICK_PERSIST_END_SECS_OF_DAY_IST..86_000,
            m_len in 1u32..7_200,
            wall_off in 0u32..86_400,
            wm_off in 0u32..86_400,
            tick_off in 0u32..86_400,
        ) {
            let m_end = (m_start + m_len).min(86_399);
            let plan = ClosePlan::build(DAY, reg, Some((m_start, m_end)));
            let wall = DAY_START + wall_off;
            let watermark = DAY_START + wm_off;
            let cutoff = plan.cutoff(wall, watermark);
            if cutoff == 0 {
                return Ok(());
            }
            prop_assert!(cutoff <= DAY_START + 86_400);
            // The qualifying session ended at least the delay before the wall.
            let qualified = plan
                .sessions()
                .iter()
                .any(|&(s, e)| e + CLOSE_SEAL_DELAY_SECS <= wall && watermark >= s);
            prop_assert!(qualified, "a session qualified");
            // A tick of any session not yet due is never inside a sealed bucket.
            let tick = DAY_START + tick_off;
            for &(s, e) in plan.sessions() {
                let due = e + CLOSE_SEAL_DELAY_SECS <= wall && watermark >= s;
                if !due && tick >= s && tick < e {
                    for tf in TfIndex::ALL {
                        let end = tf.bucket_end(tf.bucket_start(tick));
                        prop_assert!(
                            end > cutoff,
                            "{tf:?} bucket ending {end} holds a later-session tick {tick} but cutoff is {cutoff}"
                        );
                    }
                }
            }
        }
    }
}
