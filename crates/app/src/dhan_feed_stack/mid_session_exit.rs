//! Whether a lane exit falls inside a session, Muhurat date included (audit
//! finding 3, 2026-11-14).
//!
//! An exit inside a session withholds the open candle buckets (each is
//! missing the trades after the exit); an exit after the session force-seals
//! them (no trade can still arrive). [`super::is_mid_session_exit`] knew only
//! the regular window, 09:00 to 15:40 IST, so an exit during a Muhurat
//! evening session (a deploy or restart at 18:45 IST on the Diwali date)
//! force-sealed every open bucket and wrote truncated bars as complete ones.
//!
//! The window here is the regular one, plus, on an IST date the trading
//! calendar names as Muhurat (`[[trading.muhurat_trading_dates]]`, the same
//! per-date source the B1 change gave the writers and the spill replay,
//! `session_window::row_day_had_muhurat`), that date's configured session
//! `[session_start_ist, session_end_ist)`. A Muhurat date whose times are not
//! published falls back to the writers' fixed evening window, 18:00 to 19:30
//! (`MUHURAT_PERSIST_*`).
//!
//! The window ends where the fold stops admitting trades, not at the
//! announced close. The regular window ends at 15:40, which IS the fold's
//! close (`tf_index::MARKET_CLOSE_SECS_OF_DAY_IST` = 56,400, the closing
//! auction's end since the 2026-08-03 session change, equal to the writers'
//! persist end `TICK_PERSIST_END`), ten minutes after the 15:30 continuous
//! close. On a Muhurat date the fold admits evening trades over the fixed
//! `[18:00, 19:30)` window whatever the configured times, so a configured
//! session overlapping that window gives exactly that window (audit round 2,
//! 2026-11-14, and round 4, the same day): an exit at 19:02 after an 18:00
//! to 19:00 session withholds the buckets whose trades the fold still takes,
//! as an exit at 15:32 does on a regular day; an exit at 18:10 before an
//! 18:15 start withholds the buckets the fold opened from 18:00 (the pre-open
//! packets); and an exit at 19:35 after a session configured to 19:45, or at
//! 15:45 after one configured 14:30 to 16:00, force-seals, because the fold
//! closed at 19:30 (or 15:40) and nothing can still complete those buckets.
//! The window is the fold's admission, never the configured times: those
//! only say whether the evening window is used. After that end an exit
//! force-seals (B1
//! review, 2026-11-13: an exit after the close must force-seal the
//! session's last bars, or they are lost for good). Neither window adds a
//! grace past the fold's close: an exit at 19:30:02 force-seals as one at
//! 15:40:02 does, and a trade stamped in the last seconds and still in
//! flight is missing from both (a limit shared with the regular day, not a
//! Muhurat gap; audit round 3, 2026-11-14). A configured session
//! that does not overlap the evening window (the 2026 shape, 14:05 to
//! 15:40, inside the regular day) adds nothing: the regular window already
//! covers what the fold admits of it.
//!
//! O(1): two compares inside the regular window (no calendar probe), and
//! outside it one date conversion and at most two hash probes. No allocation.
//! Cold: called once, at a lane exit.

use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS_I64, MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
    MUHURAT_PERSIST_START_SECS_OF_DAY_IST, SECONDS_PER_DAY,
};

/// Nanoseconds per second, for the per-date Muhurat lookup that takes nanos.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// True when an exit at IST `secs_of_day` is inside the regular window or
/// inside `muhurat_session` (`[start, end)`, IST seconds of day; `None` on a
/// day with no Muhurat session). Pure.
#[must_use]
pub const fn is_mid_session_exit_in(secs_of_day: u64, muhurat_session: Option<(u32, u32)>) -> bool {
    if super::is_mid_session_exit(secs_of_day) {
        return true;
    }
    match muhurat_session {
        Some((start, end)) => secs_of_day >= start as u64 && secs_of_day < end as u64,
        None => false,
    }
}

/// The exit window a configured Muhurat session `[start, end)` adds to the
/// regular one: where the fold admits that session's trades. A session
/// overlapping the fold's `[MUHURAT_PERSIST_START, MUHURAT_PERSIST_END)`
/// evening window gives exactly that window (both ends, whatever the
/// configured times); any other session gives `None`, because the fold
/// admits none of it outside the regular window, which
/// [`super::is_mid_session_exit`] already covers. Pure, O(1).
#[must_use]
pub const fn exit_window_for_session(start: u32, end: u32) -> Option<(u32, u32)> {
    if start < MUHURAT_PERSIST_END_SECS_OF_DAY_IST && end > MUHURAT_PERSIST_START_SECS_OF_DAY_IST {
        Some((
            MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
            MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
        ))
    } else {
        None
    }
}

/// The Muhurat session window of the IST date of `ist_epoch_secs` (IST wall
/// seconds since the epoch), or `None` when that date has no Muhurat session
/// or its configured session does not overlap the fold's evening window.
/// The date's Muhurat flag comes from the installed calendar (else the
/// process flag), its times from the same calendar (mapped to the fold's
/// window by [`exit_window_for_session`]), else the writers' fixed evening
/// window.
#[must_use]
pub fn muhurat_exit_window(ist_epoch_secs: i64) -> Option<(u32, u32)> {
    let ist_nanos = ist_epoch_secs.saturating_mul(NANOS_PER_SECOND);
    if !tickvault_common::session_window::row_day_had_muhurat(ist_nanos) {
        return None;
    }
    let configured = chrono::DateTime::from_timestamp(ist_epoch_secs, 0)
        .map(|t| t.date_naive())
        .and_then(tickvault_common::market_hours::installed_muhurat_session_secs);
    match configured {
        Some((start, end)) => exit_window_for_session(start, end),
        None => Some((
            MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
            MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
        )),
    }
}

/// [`is_mid_session_exit_in`] for an IST epoch second, with that date's
/// Muhurat session from the calendar. The calendar is asked only outside the
/// regular window.
#[must_use]
pub fn is_mid_session_exit_at(ist_epoch_secs: i64) -> bool {
    let secs_of_day = ist_epoch_secs
        .rem_euclid(i64::from(SECONDS_PER_DAY))
        .unsigned_abs();
    super::is_mid_session_exit(secs_of_day)
        || is_mid_session_exit_in(secs_of_day, muhurat_exit_window(ist_epoch_secs))
}

/// [`is_mid_session_exit_at`] for the wall clock. What both lane exits call.
#[must_use]
pub fn is_mid_session_exit_now() -> bool {
    is_mid_session_exit_at(
        chrono::Utc::now()
            .timestamp()
            .saturating_add(IST_UTC_OFFSET_SECONDS_I64),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn hm(h: u64, m: u64) -> u64 {
        h * 3_600 + m * 60
    }

    #[test]
    fn test_is_mid_session_exit_in_adds_the_muhurat_session_only_on_its_day() {
        let evening = Some((18 * 3_600, 19 * 3_600));
        // Audit finding 3: 18:45 on a Muhurat date withholds; the same clock
        // on any other date force-seals.
        assert!(is_mid_session_exit_in(hm(18, 45), evening));
        assert!(!is_mid_session_exit_in(hm(18, 45), None));
        // `is_mid_session_exit_in` takes the window as given; the widening to
        // the fold's end is `exit_window_for_session`'s (tested below).
        assert!(is_mid_session_exit_in(hm(18, 59) + 59, evening));
        assert!(!is_mid_session_exit_in(hm(19, 0), evening));
        assert!(!is_mid_session_exit_in(hm(17, 59) + 59, evening));
        // The regular window is unchanged on both kinds of day.
        for day in [None, evening] {
            assert!(!is_mid_session_exit_in(hm(8, 59) + 59, day));
            assert!(is_mid_session_exit_in(hm(9, 0), day));
            assert!(is_mid_session_exit_in(hm(15, 39) + 59, day));
            assert!(!is_mid_session_exit_in(hm(15, 40), day));
        }
    }

    #[test]
    fn test_exit_window_for_session_is_exactly_where_the_fold_admits() {
        let fold = Some((
            MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
            MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
        ));
        let w = |s: u64, e: u64| exit_window_for_session(s as u32, e as u32);
        // Audit round 2: an 18:00 to 19:00 session ends at the fold's 19:30.
        assert_eq!(w(hm(18, 0), hm(19, 0)), fold);
        // Audit round 4: a later start opens at the fold's 18:00 (pre-open
        // packets fold from there), and a later end is cut back to 19:30.
        assert_eq!(w(hm(18, 15), hm(19, 15)), fold);
        assert_eq!(w(hm(18, 15), hm(19, 45)), fold);
        assert_eq!(w(hm(17, 30), hm(19, 0)), fold);
        assert_eq!(w(hm(19, 29), hm(20, 0)), fold);
        // A session the fold admits none of outside the regular window adds
        // nothing: the 2026 daytime shape, one running past 15:40, one
        // ending exactly at 18:00 and one starting at 19:30.
        assert_eq!(w(hm(14, 5), hm(15, 40)), None);
        assert_eq!(w(hm(14, 30), hm(16, 0)), None);
        assert_eq!(w(hm(17, 0), hm(18, 0)), None);
        assert_eq!(w(hm(19, 30), hm(20, 30)), None);
    }

    /// Audit round 4 (2026-11-14): no window this module returns admits an
    /// exit at a second the fold refuses, nor misses one it admits on the
    /// Muhurat evening, for every session shape on a 15-minute grid.
    #[test]
    fn test_exit_window_never_outlives_or_starts_after_the_fold() {
        let (fold_start, fold_end) = (
            u64::from(MUHURAT_PERSIST_START_SECS_OF_DAY_IST),
            u64::from(MUHURAT_PERSIST_END_SECS_OF_DAY_IST),
        );
        for start in (0..86_400u64).step_by(900) {
            for end in ((start + 900)..=86_400u64).step_by(900) {
                let window = exit_window_for_session(start as u32, end as u32);
                for secs in (0..86_400u64).step_by(60) {
                    let fold_admits = super::super::is_mid_session_exit(secs)
                        || (secs >= fold_start && secs < fold_end);
                    if is_mid_session_exit_in(secs, window) {
                        assert!(fold_admits, "{start}-{end}: exit at {secs} withholds");
                    } else if window.is_some() {
                        assert!(!fold_admits, "{start}-{end}: exit at {secs} seals");
                    }
                }
            }
        }
    }

    #[test]
    fn test_is_mid_session_exit_now_reads_the_wall_clock_without_panicking() {
        // The wall clock decides the answer; only that it returns is checked
        // here (the calendar-driven cases are in
        // `tests/muhurat_mid_session_exit.rs`, their own binary).
        let _verdict: bool = is_mid_session_exit_now();
        // A negative or huge stamp cannot panic.
        let _ = is_mid_session_exit_at(i64::MIN);
        let _ = is_mid_session_exit_at(i64::MAX);
        // A stamp before 1970 carries no calendar date: only the process flag
        // can answer, and then with the fixed evening window.
        if let Some(window) = muhurat_exit_window(-1) {
            assert_eq!(
                window,
                (
                    MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
                    MUHURAT_PERSIST_END_SECS_OF_DAY_IST
                )
            );
        }
    }
}
