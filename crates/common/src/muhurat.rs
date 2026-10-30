//! Muhurat-session persist flag — CCL-06 (permutation-coverage audit §140).
//!
//! On a Diwali Muhurat date NSE trades a ceremonial ~1-hour evening session
//! (~18:00–19:30 IST). Every persistence window in this system names the
//! REGULAR session, `[09:00, 15:40)` IST, so without a widening the whole
//! Muhurat session is refused: a connection that receives and stores nothing,
//! which is the audit Rule 11 false-OK.
//!
//! This module carries the boot-computed "is today a Muhurat session?" flag.
//! `main.rs` computes it from the day's calendar
//! (`TradingCalendar::is_muhurat_trading_today`, itself driven by
//! `[trading] muhurat_trading_dates` in config) and installs it once via
//! [`init_muhurat_session`].
//!
//! ## ⚠ CORRECTED 2026-09-05 — this header described a consumer that no longer
//! ## exists, and for months there was no consumer at all
//!
//! It previously said the flag exists "so the tick processor can additionally
//! accept ticks inside the Muhurat window", and that `main.rs` sets
//! `should_connect_ws = ... || is_muhurat`. Both symbols are **gone**:
//! `run_tick_processor` died with the 2026-07-17 dead-WS sweep, and
//! `should_connect_ws` has zero occurrences in the repository outside this
//! sentence. So the flag was **write-only** — installed at boot by `main.rs`
//! and read by nothing, with `MUHURAT_PERSIST_{START,END}_SECS_OF_DAY_IST`
//! sitting in the dead-const allowlist to prove it.
//!
//! That is the `scan_silence` shape the O(1) table records: a sensor wired at
//! one end. It reads greener than an absent mechanism, because the name exists
//! and the boot line runs.
//!
//! ## Who reads it now
//!
//! [`current`] is read by `session_window::row_is_in_an_open_window` and
//! `session_window::verdict`, which are what the tick, depth and candle
//! writers call. So the widening is wired end to end, and both constants are
//! live again.
//!
//! **NOT claimed: that a Muhurat session would actually be captured today.**
//! The box is force-stopped outside Mon–Fri 08:30–17:30 IST
//! (`hard_stop_guard::in_up_window`, plus the 17:30 EventBridge stop), so no
//! process exists at 18:00–19:30 to receive a frame — and the single
//! configured date, 2026-11-08, is a Sunday, when the box never starts. This
//! module makes the DATA path correct; the SCHEDULE remains an operator
//! decision, and it is the binding constraint.
//!
//! **⚠ CORRECTED 2026-10-30 — the schedule half is no longer missing.** Plan
//! item J8 (2026-10-26) starts the box for a calendar Muhurat date
//! (`aws-lambdas` `muhurat_window`), and the 2026-10-30 change (owner: "go
//! ahead and fix the muhurat risk dude") moves the session TIMES into
//! `[[trading.muhurat_trading_dates]]` as the one source the box schedule,
//! the deploy freeze and the liveness alarm arming are pinned to.
//! [`session_is_inside_capture_windows`] checks those times against this
//! module's fixed windows, and `tests/muhurat_session_calendar.rs` fails the
//! build when a configured session would be cut by them.
//!
//! ## Why a process-global transport
//!
//! The flag is computed ONCE at boot but the writers are constructed in deep,
//! separate scopes; threading a `bool` through ~20 signatures would be churn
//! for a value that never changes after boot. The globals-reading wrappers are
//! one `OnceLock` load — O(1), allocation-free, safe on the hot path.
//!
//! The window PREDICATES stay pure and take the flag as a parameter
//! (`nanos_in_any_open_window`, `verdict_in`), so every window decision is
//! unit-testable in isolation. A test that read this `OnceLock` would depend
//! on whichever other test in the same binary installed it first, and an
//! order-dependent window decision is worse than no test.

use std::sync::OnceLock;

/// Boot-set-once Muhurat flag. Production sets this exactly once after the daily
/// calendar is loaded; it is read-only thereafter.
static MUHURAT_ACTIVE: OnceLock<bool> = OnceLock::new();

/// Install the boot-computed Muhurat-session flag. Idempotent: the first call
/// wins; later calls are ignored (boot runs once). Safe to never call —
/// [`current`] then returns `false` (today's behaviour: no Muhurat widening).
// TEST-EXEMPT: covered by tests::current_before_init_is_false_then_reflects_init.
pub fn init_muhurat_session(active: bool) {
    // First call wins; a second call returns Err(rejected) — intentional
    // idempotency (boot runs once), consumed via `is_err()` so the
    // #[must_use] `Result<(), bool>` is handled explicitly (the Copy type
    // makes `drop(..)` a `dropping_copy_types` lint; `let _` a
    // `let_underscore_must_use` lint). The ignored repeat is logged so it
    // is never silent.
    if MUHURAT_ACTIVE.set(active).is_err() {
        tracing::debug!(
            rejected_value = active,
            "muhurat init called more than once; first boot value kept"
        );
    }
}

/// The current Muhurat-session flag. Returns `false` if [`init_muhurat_session`]
/// was never called (e.g. the `Indices4Only` scope, any test that does not boot
/// the calendar, or a normal trading/mock day where boot passes `false`).
#[must_use]
// TEST-EXEMPT: covered by tests::current_before_init_is_false_then_reflects_init.
pub fn current() -> bool {
    MUHURAT_ACTIVE.get().copied().unwrap_or(false)
}

/// True when a whole Muhurat session `[start, end)` (IST seconds of day, end
/// exclusive) lies inside ONE window the tick, depth and candle writers accept
/// on a Muhurat day: the regular session window
/// `[TICK_PERSIST_START_SECS_OF_DAY_IST, TICK_PERSIST_END_SECS_OF_DAY_IST)`
/// (09:00–15:40), or the fixed Muhurat evening window
/// `[MUHURAT_PERSIST_START_SECS_OF_DAY_IST, MUHURAT_PERSIST_END_SECS_OF_DAY_IST)`
/// (18:00–19:30).
///
/// The session times come from config (`[[trading.muhurat_trading_dates]]`)
/// while the writers' windows are constants, so this is the check that the
/// two agree: a session that straddles 15:40–18:00 or runs past 19:30 would
/// be partly refused by the writers. Pinned against `config/base.toml` by
/// `tests/muhurat_session_calendar.rs`, so such a session fails the build
/// instead of losing ticks on the day. Pure, O(1).
#[must_use]
pub const fn session_is_inside_capture_windows(start_secs: u32, end_secs: u32) -> bool {
    use crate::constants::{
        MUHURAT_PERSIST_END_SECS_OF_DAY_IST, MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
        TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
    };
    if start_secs >= end_secs {
        return false;
    }
    let in_regular = start_secs >= TICK_PERSIST_START_SECS_OF_DAY_IST
        && end_secs <= TICK_PERSIST_END_SECS_OF_DAY_IST;
    let in_evening = start_secs >= MUHURAT_PERSIST_START_SECS_OF_DAY_IST
        && end_secs <= MUHURAT_PERSIST_END_SECS_OF_DAY_IST;
    in_regular || in_evening
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn hm(h: u32, m: u32) -> u32 {
        h * 3600 + m * 60
    }

    #[test]
    fn session_capture_windows_accept_afternoon_and_evening_sessions() {
        // 2026 (reported): pre-open 14:05, close by 15:40.
        assert!(session_is_inside_capture_windows(hm(14, 5), hm(15, 40)));
        // 2025: 13:45-14:45.
        assert!(session_is_inside_capture_windows(hm(13, 45), hm(14, 45)));
        // 2024: 18:00-19:00, and an 18:00-19:15 style evening window.
        assert!(session_is_inside_capture_windows(hm(18, 0), hm(19, 0)));
        assert!(session_is_inside_capture_windows(hm(18, 0), hm(19, 15)));
        // Exact window edges are inside (end is exclusive on both sides).
        assert!(session_is_inside_capture_windows(hm(9, 0), hm(15, 40)));
        assert!(session_is_inside_capture_windows(hm(18, 0), hm(19, 30)));
    }

    #[test]
    fn session_capture_windows_refuse_a_session_the_writers_would_cut() {
        // Runs past the evening window's 19:30 end.
        assert!(!session_is_inside_capture_windows(hm(18, 15), hm(19, 45)));
        // Starts before the evening window opens.
        assert!(!session_is_inside_capture_windows(hm(17, 45), hm(19, 0)));
        // Straddles the 15:40-18:00 gap.
        assert!(!session_is_inside_capture_windows(hm(15, 0), hm(16, 0)));
        // Before 09:00.
        assert!(!session_is_inside_capture_windows(hm(8, 30), hm(9, 30)));
        // Empty or reversed.
        assert!(!session_is_inside_capture_windows(hm(14, 0), hm(14, 0)));
        assert!(!session_is_inside_capture_windows(hm(15, 0), hm(14, 0)));
    }

    #[test]
    fn current_before_init_is_false_then_reflects_init() {
        // NOTE: this test OWNS the OnceLock for the whole lib-test binary, and
        // the assertion below is what makes that ownership load-bearing: it can
        // only hold while this is the FIRST call in the process. Every other
        // test in this crate therefore routes through the pure forms
        // (`session_window::nanos_in_any_open_window` / `verdict_in`), which
        // take the flag as a parameter. The wrappers that DO read the global
        // are covered by `tests/muhurat_widening_wiring.rs` -- a separate
        // integration binary with its own OnceLock, precisely so installing the
        // flag there cannot race this.
        //
        // Before init → false (today's behaviour, no Muhurat widening).
        assert!(
            !current(),
            "uninitialised Muhurat flag must be false. If this fires, another \
             test in the tickvault-common LIB test binary called \
             init_muhurat_session first -- move it to its own integration test \
             binary rather than relaxing this assertion."
        );

        // After init → reflects the set value.
        init_muhurat_session(true);
        assert!(current(), "Muhurat flag must reflect the boot value (true)");

        // Set-once: a second init is ignored (first call wins).
        init_muhurat_session(false);
        assert!(current(), "first init wins — second init ignored");
    }
}
