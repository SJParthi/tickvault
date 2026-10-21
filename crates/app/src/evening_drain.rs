//! The evening WAL drain (operator Quote 31, 2026-10-21): "every waiting WAL
//! segment is replayed" the same evening.
//!
//! A WAL segment the replay still has to read (one the held unapplied ranges
//! pin: frames from an earlier process that never reached the database, or
//! this session's frames acked while QuestDB was untrusted) is replayed only
//! by a boot's catch-up drain, which runs before the sockets dial. So after
//! the close, once the day's own work is done (and the day's 1-minute
//! cross-verification has run, or 17:25 IST has passed), the drain-hold
//! supervisor asks this process for a planned restart. systemd's
//! `Restart=always` starts it again three seconds later and its catch-up
//! replays the backlog.
//!
//! # Bounds
//!
//! - At most [`EVENING_DRAIN_RESTARTS_MAX`] restarts an evening,
//!   [`EVENING_DRAIN_SPACING_SECS`] (40 minutes) apart, so at most two fall in
//!   any hour and, with a deploy restart, the unit keeps headroom under its
//!   `StartLimitBurst=5` per hour.
//! - Each restart's own catch-up is held for the whole spacing, whatever the
//!   cap or the clock says; only then is progress judged. A restart after
//!   which the backlog did not shrink ends the evening's restarts, and that
//!   verdict is written down so it cannot flip back.
//! - None after [`EVENING_DRAIN_LAST_RESTART_SECS_IST`] (05:00 IST), so the
//!   last one is judged well before the 07:30 hold ceiling.
//! - The restarted boot keeps every per-boot bound of the catch-up (the frame
//!   cap, the round cap, the memory stop, the disk floor); only its clock is
//!   [`EVENING_DRAIN_CATCHUP_BUDGET_SECS`] instead of five minutes, so lag
//!   pauses do not end it early.
//!
//! # State
//!
//! One small file, [`EVENING_DRAIN_STATE_PATH`], survives the restart: the
//! evening it belongs to, restarts so far, the last restart instant, the
//! backlog then, whether the next boot is the evening drain boot, and whether
//! the evening has given up. A missing file reads as "no restart yet"; a
//! damaged or unreadable one stops the restarts (it could not be counted).
//! Cold: read once a minute after the close and once at boot. The restarted
//! app sends its usual boot message; the shutdown message is not sent.

use std::path::Path;

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// The state file, relative to the app's working directory.
pub const EVENING_DRAIN_STATE_PATH: &str = "data/state/evening-drain.state";
/// Planned restarts allowed in one evening.
pub const EVENING_DRAIN_RESTARTS_MAX: u32 = 3;
/// Least time between two restarts, and from a restart to its progress
/// check: the old process's shutdown, the boot (up to five minutes waiting
/// for the token), the 15-minute catch-up, and a few minutes for the
/// replayed segments to settle (hostile review, 2026-10-21: 20 minutes was
/// shorter than that).
pub const EVENING_DRAIN_SPACING_SECS: i64 = 2_400;
/// 15:50 IST: no restart before this (the capture ends at 15:40).
pub const EVENING_DRAIN_FIRST_SECS_IST: u64 = 15 * 3_600 + 50 * 60;
/// 05:00 IST: no restart after this.
pub const EVENING_DRAIN_LAST_RESTART_SECS_IST: u64 = 5 * 3_600;
/// 17:25 IST: past the day's last 1-minute cross-verification attempt
/// (17:23), so a restart can no longer cut one short.
pub const EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST: u64 = 17 * 3_600 + 25 * 60;
/// The restarted boot's catch-up clock, in seconds.
pub const EVENING_DRAIN_CATCHUP_BUDGET_SECS: u64 = 900;
/// A pending flag older than this is ignored (a restart that never came).
pub const EVENING_DRAIN_PENDING_MAX_AGE_SECS: i64 = 900;

const _: () = assert!(EVENING_DRAIN_CATCHUP_BUDGET_SECS < EVENING_DRAIN_SPACING_SECS as u64);
const _: () = assert!(
    EVENING_DRAIN_LAST_RESTART_SECS_IST + (EVENING_DRAIN_SPACING_SECS as u64)
        < crate::drain_hold::DRAIN_HOLD_CEILING_SECS_IST
);
const _: () = assert!(EVENING_DRAIN_FIRST_SECS_IST > crate::drain_hold::DRAIN_HOLD_START_SECS_IST);
// At most two restarts start in any hour, leaving systemd (5 an hour) room
// for a deploy and a crash.
const _: () = assert!(3_600 / EVENING_DRAIN_SPACING_SECS < 2);

static SELF_RESTART: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Asks `main` for a planned restart. One stored permit, so a request made
/// while no shutdown wait is running is taken by the next one. O(1).
pub fn request_self_restart() {
    SELF_RESTART.notify_one();
}

/// Resolves when [`request_self_restart`] was called. Cancel-safe.
pub async fn self_restart_requested() {
    SELF_RESTART.notified().await;
}

/// What survives a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EveningState {
    /// The evening (its trading day, days from the common era); 0 = none.
    pub day: i32,
    /// Restarts made that evening.
    pub restarts: u32,
    /// When the last one was requested (unix seconds).
    pub last_unix: i64,
    /// The backlog (segments) when it was requested.
    pub backlog_before: u64,
    /// When the next boot was marked as the evening drain boot; 0 = not.
    pub pending_unix: i64,
    /// The evening gave up (cap, window or no progress): no more restarts.
    pub gave_up: bool,
}

impl EveningState {
    /// `v1 day restarts last backlog pending gave_up`. Pure.
    #[must_use]
    pub fn to_line(&self) -> String {
        format!(
            "v1 {} {} {} {} {} {}\n",
            self.day,
            self.restarts,
            self.last_unix,
            self.backlog_before,
            self.pending_unix,
            u8::from(self.gave_up)
        )
    }

    /// The inverse of [`Self::to_line`]; anything else is `None`. Pure.
    #[must_use]
    pub fn from_line(line: &str) -> Option<Self> {
        let mut it = line.split_ascii_whitespace();
        if it.next()? != "v1" {
            return None;
        }
        let state = Self {
            day: it.next()?.parse().ok()?,
            restarts: it.next()?.parse().ok()?,
            last_unix: it.next()?.parse().ok()?,
            backlog_before: it.next()?.parse().ok()?,
            pending_unix: it.next()?.parse().ok()?,
            gave_up: match it.next()? {
                "0" => false,
                "1" => true,
                _ => return None,
            },
        };
        it.next().is_none().then_some(state)
    }
}

/// Reads the state: the default when the file does not exist, `None` when it
/// cannot be read or parsed (the caller then makes no restart: a count it
/// cannot read is a cap it cannot keep).
#[must_use]
pub fn load_state(path: &Path) -> Option<EveningState> {
    match std::fs::read_to_string(path) {
        Ok(s) => EveningState::from_line(&s),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(EveningState::default()),
        Err(_) => None,
    }
}

/// Writes the state (temp file and rename).
///
/// # Errors
/// The write or the rename failed.
pub fn store_state(path: &Path, state: &EveningState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, state.to_line())?;
    std::fs::rename(&tmp, path)
}

/// Whether a restart may be requested at `secs_ist` (15:50 to 05:00, wrapping
/// midnight). O(1).
#[must_use]
pub const fn in_restart_window(secs_ist: u64) -> bool {
    secs_ist >= EVENING_DRAIN_FIRST_SECS_IST || secs_ist < EVENING_DRAIN_LAST_RESTART_SECS_IST
}

/// One evening decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EveningStep {
    /// No backlog, or it could not be read: nothing to do, nothing held.
    Idle,
    /// A backlog, and a restart will come (the day's own work, or the spacing,
    /// is still in the way): it holds the box.
    Wait,
    /// Restart now: it holds the box.
    Restart,
    /// No more restarts tonight (cap, window, or no progress): it no longer
    /// holds the box; the next boot replays it as before.
    GaveUp,
}

impl EveningStep {
    /// Whether the backlog keeps the drain hold. O(1).
    #[must_use]
    pub const fn holds(self) -> bool {
        matches!(self, Self::Wait | Self::Restart)
    }
}

/// Whether the day's 1-minute cross-verification can no longer be cut short
/// by a restart: its day marker exists, or it is past 17:25 IST (its last
/// attempt ends by 17:23), including the hours after midnight. O(1).
#[must_use]
pub const fn crossverify_settled(marker_written: bool, secs_ist: u64) -> bool {
    marker_written
        || secs_ist >= EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST
        || secs_ist < crate::drain_hold::DRAIN_HOLD_CEILING_SECS_IST
}

/// Decides the evening's next step. `other_done` is the day's own work done
/// (everything but the backlog); `xverify_settled` is
/// [`crossverify_settled`]; `backlog` is the pinned segment count, `None`
/// when unreadable. Pure, O(1).
///
/// Order matters (hostile review, 2026-10-21): a restart's own catch-up is
/// held for the whole spacing before the cap or the clock can end the
/// evening, and progress is judged only after it.
#[must_use]
pub fn evening_step(
    state: &EveningState,
    day: i32,
    other_done: bool,
    xverify_settled: bool,
    backlog: Option<u64>,
    now_unix: i64,
    secs_ist: u64,
) -> EveningStep {
    let tonight = state.day == day;
    if tonight && state.gave_up {
        return EveningStep::GaveUp;
    }
    let restarts = if tonight { state.restarts } else { 0 };
    let since_last = now_unix.saturating_sub(state.last_unix);
    let catching_up = restarts > 0 && since_last < EVENING_DRAIN_SPACING_SECS;
    let backlog = match backlog {
        // A scan that raced the catch-up's segment moves keeps the hold.
        None if catching_up => return EveningStep::Wait,
        None | Some(0) => return EveningStep::Idle,
        Some(n) => n,
    };
    if catching_up {
        return EveningStep::Wait;
    }
    if restarts > 0 && backlog >= state.backlog_before {
        return EveningStep::GaveUp;
    }
    if restarts >= EVENING_DRAIN_RESTARTS_MAX || !in_restart_window(secs_ist) {
        return EveningStep::GaveUp;
    }
    if !other_done || !xverify_settled {
        return EveningStep::Wait;
    }
    EveningStep::Restart
}

/// The state to write for a restart requested now. Pure.
#[must_use]
pub fn state_for_restart(
    state: &EveningState,
    day: i32,
    backlog: u64,
    now_unix: i64,
) -> EveningState {
    let restarts = if state.day == day { state.restarts } else { 0 };
    EveningState {
        day,
        restarts: restarts.saturating_add(1),
        last_unix: now_unix,
        backlog_before: backlog,
        pending_unix: now_unix,
        gave_up: false,
    }
}

/// The state to write when the evening gives up, so the verdict cannot flip
/// back while the backlog count moves. Pure.
#[must_use]
pub fn state_for_give_up(state: &EveningState, day: i32) -> EveningState {
    if state.day == day {
        EveningState {
            gave_up: true,
            ..*state
        }
    } else {
        EveningState {
            day,
            gave_up: true,
            ..EveningState::default()
        }
    }
}

/// The catch-up clock for this boot: [`EVENING_DRAIN_CATCHUP_BUDGET_SECS`]
/// when this is the evening drain boot (a fresh pending flag, inside the
/// restart window or the hour after it), else `standard`. Pure.
#[must_use]
pub fn catchup_budget_for(
    state: &EveningState,
    standard: u64,
    now_unix: i64,
    secs_ist: u64,
) -> u64 {
    let age = now_unix.saturating_sub(state.pending_unix);
    let fresh = state.pending_unix != 0 && (0..=EVENING_DRAIN_PENDING_MAX_AGE_SECS).contains(&age);
    let evening = in_restart_window(secs_ist)
        || secs_ist
            < EVENING_DRAIN_LAST_RESTART_SECS_IST + EVENING_DRAIN_PENDING_MAX_AGE_SECS as u64;
    if fresh && evening {
        standard.max(EVENING_DRAIN_CATCHUP_BUDGET_SECS)
    } else {
        standard
    }
}

/// [`catchup_budget_for`] on the state file, clearing its pending flag so it
/// applies to one boot only. Called once per lane start, cold.
#[must_use]
// TEST-EXEMPT: file I/O around catchup_budget_for, which is unit-tested (test_catchup_budget_for_*) and on store_state/load_state.
pub fn budget_or(standard: u64) -> u64 {
    let path = Path::new(EVENING_DRAIN_STATE_PATH);
    let Some(state) = load_state(path) else {
        return standard;
    };
    if state.pending_unix == 0 {
        return standard;
    }
    let budget = catchup_budget_for(
        &state,
        standard,
        chrono::Utc::now().timestamp(),
        crate::dhan_feed_stack::now_ist_secs_of_day(),
    );
    let cleared = EveningState {
        pending_unix: 0,
        ..state
    };
    if let Err(err) = store_state(path, &cleared) {
        error!(
            code = ErrorCode::DrainHold01NotWritten.code_str(),
            source = "evening_drain_state",
            error = %err,
            "evening drain: the pending flag could not be cleared; it ages out in 15 minutes"
        );
    }
    if budget > standard {
        info!(
            budget_secs = budget,
            restart = state.restarts,
            backlog_before = state.backlog_before,
            "evening drain boot: the WAL catch-up takes the longer evening clock"
        );
    }
    budget
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i32 = 739_910;
    const NOW: i64 = 1_792_000_000;
    /// 18:00 IST: after the close and past the cross-verification.
    const EVENING: u64 = 18 * 3_600;
    const SPACING: i64 = EVENING_DRAIN_SPACING_SECS;

    fn tonight(restarts: u32, last: i64, backlog_before: u64) -> EveningState {
        EveningState {
            day: DAY,
            restarts,
            last_unix: last,
            backlog_before,
            ..EveningState::default()
        }
    }

    /// `evening_step` with the day's own work done and the cross-check clear.
    fn step(s: &EveningState, backlog: Option<u64>, now: i64, secs: u64) -> EveningStep {
        evening_step(s, DAY, true, true, backlog, now, secs)
    }

    #[test]
    fn test_store_state_load_state_to_line_from_line_round_trip() {
        let dir = std::env::temp_dir().join(format!("tv-evening-drain-{}", std::process::id()));
        let path = dir.join("evening-drain.state");
        let state = EveningState {
            day: DAY,
            restarts: 2,
            last_unix: NOW,
            backlog_before: 400,
            pending_unix: NOW,
            gave_up: true,
        };
        store_state(&path, &state).expect("write");
        assert_eq!(load_state(&path), Some(state));
        assert_eq!(EveningState::from_line(&state.to_line()), Some(state));
        // Missing reads as no restart yet; damaged reads as unknown.
        assert_eq!(
            load_state(&dir.join("absent")),
            Some(EveningState::default())
        );
        std::fs::write(&path, "v1 1 2 3").expect("write");
        assert_eq!(load_state(&path), None);
        assert_eq!(EveningState::from_line("v2 1 2 3 4 5 0"), None);
        assert_eq!(EveningState::from_line("v1 1 2 3 4 5 2"), None);
        assert_eq!(EveningState::from_line("v1 1 2 3 4 5 0 7"), None);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_evening_step_idles_and_holds_nothing_without_a_backlog() {
        let s = EveningState::default();
        assert_eq!(step(&s, None, NOW, EVENING), EveningStep::Idle);
        assert_eq!(step(&s, Some(0), NOW, EVENING), EveningStep::Idle);
        assert!(!EveningStep::Idle.holds());
    }

    #[test]
    fn evening_step_waits_for_the_days_work_and_the_cross_check_then_restarts() {
        let s = EveningState::default();
        let not_done = evening_step(&s, DAY, false, true, Some(9), NOW, EVENING);
        assert_eq!(not_done, EveningStep::Wait);
        let xverify_running = evening_step(&s, DAY, true, false, Some(9), NOW, EVENING);
        assert_eq!(xverify_running, EveningStep::Wait);
        assert_eq!(step(&s, Some(9), NOW, EVENING), EveningStep::Restart);
        assert!(EveningStep::Wait.holds() && EveningStep::Restart.holds());
    }

    #[test]
    fn evening_step_holds_each_restarts_catch_up_then_judges_progress() {
        let s = tonight(1, NOW, 100);
        // Inside the spacing: held, even when the scan raced and read nothing.
        assert_eq!(step(&s, Some(100), NOW + 60, EVENING), EveningStep::Wait);
        assert_eq!(step(&s, None, NOW + 60, EVENING), EveningStep::Wait);
        let later = NOW + SPACING;
        assert_eq!(step(&s, Some(100), later, EVENING), EveningStep::GaveUp);
        assert_eq!(step(&s, Some(60), later, EVENING), EveningStep::Restart);
        assert!(!EveningStep::GaveUp.holds());
    }

    #[test]
    fn evening_step_holds_the_last_restart_and_one_past_the_window() {
        // The third restart's own catch-up is held although the cap is hit.
        let capped = tonight(EVENING_DRAIN_RESTARTS_MAX, NOW, 100);
        assert_eq!(
            step(&capped, Some(50), NOW + 60, EVENING),
            EveningStep::Wait
        );
        assert_eq!(
            step(&capped, Some(50), NOW + SPACING, EVENING),
            EveningStep::GaveUp
        );
        // A restart at 04:55 is held after 05:00, then the window ends it.
        let late = tonight(1, NOW, 100);
        assert_eq!(
            step(&late, Some(50), NOW + 600, 5 * 3_600 + 300),
            EveningStep::Wait
        );
        let after = step(&late, Some(50), NOW + SPACING, 5 * 3_600 + 2_100);
        assert_eq!(after, EveningStep::GaveUp);
    }

    #[test]
    fn evening_step_stops_outside_the_window_and_after_giving_up() {
        let fresh = EveningState::default();
        assert_eq!(step(&fresh, Some(5), NOW, 6 * 3_600), EveningStep::GaveUp);
        assert_eq!(step(&fresh, Some(5), NOW, 15 * 3_600), EveningStep::GaveUp);
        assert_eq!(step(&fresh, Some(5), NOW, 2 * 3_600), EveningStep::Restart);
        // A latched give-up does not flip back when the count moves.
        let gave_up = state_for_give_up(&tonight(1, NOW, 100), DAY);
        assert_eq!(
            step(&gave_up, Some(10), NOW + 2 * SPACING, EVENING),
            EveningStep::GaveUp
        );
        // Last evening's count and verdict do not carry over.
        let yesterday = EveningState {
            day: DAY - 1,
            ..gave_up
        };
        assert_eq!(
            step(&yesterday, Some(5), NOW, EVENING),
            EveningStep::Restart
        );
    }

    #[test]
    fn state_for_restart_counts_per_evening() {
        let s = state_for_restart(&EveningState::default(), DAY, 70, NOW);
        let want = EveningState {
            day: DAY,
            restarts: 1,
            last_unix: NOW,
            backlog_before: 70,
            pending_unix: NOW,
            gave_up: false,
        };
        assert_eq!(s, want);
        let s2 = state_for_restart(&s, DAY, 40, NOW + SPACING);
        assert_eq!(s2.restarts, 2);
        let next_evening = state_for_restart(&s2, DAY + 1, 40, NOW + 86_400);
        assert_eq!(next_evening.restarts, 1);
    }

    #[test]
    fn state_for_give_up_keeps_tonights_count_and_resets_a_new_evening() {
        let s = tonight(2, NOW, 30);
        let g = state_for_give_up(&s, DAY);
        assert!(g.gave_up);
        assert_eq!((g.restarts, g.last_unix), (2, NOW));
        let fresh = state_for_give_up(&s, DAY + 1);
        assert_eq!(
            (fresh.day, fresh.restarts, fresh.gave_up),
            (DAY + 1, 0, true)
        );
    }

    #[test]
    fn crossverify_settled_by_marker_or_clock() {
        assert!(!crossverify_settled(false, 16 * 3_600));
        assert!(crossverify_settled(true, 16 * 3_600));
        assert!(crossverify_settled(
            false,
            EVENING_DRAIN_XVERIFY_CLEAR_SECS_IST
        ));
        assert!(crossverify_settled(false, 2 * 3_600), "after midnight");
    }

    #[test]
    fn test_catchup_budget_for_only_a_fresh_evening_pending_flag() {
        let pending = EveningState {
            pending_unix: NOW,
            ..EveningState::default()
        };
        let budget = EVENING_DRAIN_CATCHUP_BUDGET_SECS;
        assert_eq!(catchup_budget_for(&pending, 300, NOW + 30, EVENING), budget);
        // Stale flag, no flag, or a morning boot: unchanged.
        let stale = NOW + EVENING_DRAIN_PENDING_MAX_AGE_SECS + 1;
        assert_eq!(catchup_budget_for(&pending, 300, stale, EVENING), 300);
        assert_eq!(
            catchup_budget_for(&EveningState::default(), 300, NOW, EVENING),
            300
        );
        assert_eq!(catchup_budget_for(&pending, 20, NOW + 30, 10 * 3_600), 20);
        // A flag stamped in the future is not fresh.
        assert_eq!(catchup_budget_for(&pending, 300, NOW - 5, EVENING), 300);
    }

    #[test]
    fn in_restart_window_wraps_midnight() {
        assert!(in_restart_window(EVENING_DRAIN_FIRST_SECS_IST));
        assert!(in_restart_window(23 * 3_600));
        assert!(in_restart_window(0));
        assert!(!in_restart_window(EVENING_DRAIN_LAST_RESTART_SECS_IST));
        assert!(!in_restart_window(EVENING_DRAIN_FIRST_SECS_IST - 1));
    }

    #[tokio::test]
    async fn test_request_self_restart_resolves_a_later_self_restart_requested() {
        request_self_restart();
        tokio::time::timeout(std::time::Duration::from_secs(1), self_restart_requested())
            .await
            .expect("the stored permit resolves the wait");
    }

    #[test]
    fn the_lane_and_main_are_wired_to_the_evening_drain() {
        let lane = include_str!("dhan_feed_stack.rs");
        assert!(lane.contains(
            "crate::evening_drain::budget_or(wal_catchup_budget_secs(now_ist_secs_of_day()))"
        ));
        let main = include_str!("main.rs");
        assert!(main.contains("tickvault_app::evening_drain::self_restart_requested()"));
        let hold = include_str!("drain_hold.rs");
        assert!(hold.contains("crate::evening_drain::request_self_restart()"));
        assert!(hold.contains("crate::evening_drain::state_for_give_up("));
        assert!(hold.contains("crate::evening_drain::crossverify_settled("));
    }
}
