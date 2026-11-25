//! When a WAL replay paces itself on the writer threads (audit R1,
//! 2026-10-03), made calendar-aware (2026-11-15 audit, live-5).
//!
//! Inside the 09:00 to 15:40 IST capture window the replay runs unpaced: it
//! runs before the sockets dial, so a wait there is live data nobody
//! receives. That reason holds only on a session day. On Saturday 14 Nov
//! 2026 four deploy boots started the box at about 14:23 IST; the boot refold
//! ran unpaced, the writer fell behind, 864,000 rows were rescued to a spill
//! file with 576 coded ERROR lines, and each short-lived process re-posted
//! the same 230 MB spill file and left one more waiting WAL segment. On a
//! weekend or NSE holiday there is no live data to protect, so the replay is
//! paced there with the full catch-up budget, as after the close.
//!
//! The caller passes whether today is a session day. Production passes the
//! trading calendar's verdict, or `true` when no calendar is installed, so a
//! box with no calendar behaves exactly as before. A Muhurat date is an NSE
//! holiday in the calendar: its daytime replay is paced, and its evening
//! session is outside the window and was paced already.
//!
//! Honest limit: the repeated spill re-post and the WAL segment each
//! short-lived deploy boot leaves are about short process lifetimes; this
//! does not address them. Pure, O(1), no allocation.

use std::time::{Duration, Instant};

use tickvault_common::constants::{
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};

use super::{WAL_CATCHUP_BUDGET_SECS, wal_catchup_budget_secs};

/// Until when a replay started at `ist_secs_of_day` paces itself, given
/// whether today is a session day. `None` (unpaced) only inside the capture
/// window on a session day. Inside the window on any other day: the full
/// catch-up budget. Outside the window: `wal_catchup_budget_secs`, unchanged.
#[must_use]
pub(super) fn wal_replay_pace_until_on(
    ist_secs_of_day: u64,
    now: Instant,
    session_day: bool,
) -> Option<Instant> {
    let start = TICK_PERSIST_START_SECS_OF_DAY_IST as u64;
    let end = TICK_PERSIST_END_SECS_OF_DAY_IST as u64;
    let in_window = ist_secs_of_day >= start && ist_secs_of_day < end;
    if in_window && session_day {
        return None;
    }
    let budget = if in_window {
        WAL_CATCHUP_BUDGET_SECS
    } else {
        wal_catchup_budget_secs(ist_secs_of_day)
    };
    now.checked_add(Duration::from_secs(budget))
}

#[cfg(test)]
mod tests {
    use super::super::WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST;
    use super::*;

    /// Moved here from `dhan_feed_stack.rs` (2026-11-15): on a session day the
    /// capture window is unpaced, and outside it the catch-up budget applies.
    #[test]
    fn test_wal_replay_pace_until_on_is_none_inside_the_capture_window() {
        let now = Instant::now();
        let start = TICK_PERSIST_START_SECS_OF_DAY_IST as u64;
        let end = TICK_PERSIST_END_SECS_OF_DAY_IST as u64;
        assert_eq!(
            wal_replay_pace_until_on(start, now, true),
            None,
            "09:00 is live"
        );
        assert_eq!(wal_replay_pace_until_on(end - 1, now, true), None);
        // After the close: the full catch-up budget.
        assert_eq!(
            wal_replay_pace_until_on(end, now, true),
            Some(now + Duration::from_secs(WAL_CATCHUP_BUDGET_SECS))
        );
        // 08:30 pre-open: never past the 08:58 stop.
        let at_0830 = 8 * 3_600 + 30 * 60;
        let until = wal_replay_pace_until_on(at_0830, now, true).expect("pre-open is paced");
        assert!(
            until.duration_since(now).as_secs()
                <= WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST.saturating_sub(at_0830)
        );
    }

    /// 2026-11-15 audit (live-5): the Saturday 14 Nov 14:23 IST deploy boot
    /// refolded unpaced and rescued 864,000 rows to the spill file. On a day
    /// that is not a session day the window is paced with the full catch-up
    /// budget; a weekday 14:23 is unchanged. Bites: with the `session_day`
    /// check removed, Saturday 14:23 returns `None`.
    #[test]
    fn test_regression_wal_replay_pace_until_on_paces_a_weekend_boot_in_the_window() {
        let now = Instant::now();
        let at_1423 = 14 * 3_600 + 23 * 60;
        assert_eq!(
            wal_replay_pace_until_on(at_1423, now, false),
            Some(now + Duration::from_secs(WAL_CATCHUP_BUDGET_SECS)),
            "a weekend or holiday 14:23 boot is paced"
        );
        assert_eq!(
            wal_replay_pace_until_on(at_1423, now, true),
            None,
            "a trading-day 14:23 boot is not"
        );
        // Outside the window a non-session day keeps the usual budget.
        let at_0830 = 8 * 3_600 + 30 * 60;
        assert_eq!(
            wal_replay_pace_until_on(at_0830, now, false),
            wal_replay_pace_until_on(at_0830, now, true)
        );
    }
}
