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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_before_init_is_false_then_reflects_init() {
        // NOTE: a single test owns the OnceLock to avoid cross-test races.
        // Before init → false (today's behaviour, no Muhurat widening).
        assert!(!current(), "uninitialised Muhurat flag must be false");

        // After init → reflects the set value.
        init_muhurat_session(true);
        assert!(current(), "Muhurat flag must reflect the boot value (true)");

        // Set-once: a second init is ignored (first call wins).
        init_muhurat_session(false);
        assert!(current(), "first init wins — second init ignored");
    }
}
