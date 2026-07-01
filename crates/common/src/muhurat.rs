//! Muhurat-session persist flag — CCL-06 (permutation-coverage audit §140).
//!
//! On a Diwali Muhurat date the live feed CONNECTS (`main.rs` sets
//! `should_connect_ws = ... || is_muhurat`), but the tick-processor persist
//! gate only accepts the regular `[09:00, 15:30)` IST window, so the whole ~1h
//! evening Muhurat session (~18:00–19:30 IST) was silently DROPPED before
//! persist/seal/broadcast — a live connection storing ZERO data (audit Rule 11
//! false-OK). This module carries the boot-computed "is today a Muhurat
//! session?" flag so the tick processor can additionally accept ticks inside
//! the Muhurat window (`MUHURAT_PERSIST_*_SECS_OF_DAY_IST`).
//!
//! ## Why a process-global transport
//!
//! The flag is computed ONCE at boot from the day's calendar
//! (`TradingCalendar::is_muhurat_trading_today`), but the tick processor is
//! spawned in a deep, separate scope of `crates/app/src/main.rs` — threading a
//! `bool` through ~20 function signatures would be huge churn. Instead boot
//! calls [`init_muhurat_session`] once, and each spawn site reads [`current`]
//! to obtain the flag it passes EXPLICITLY into `run_tick_processor(..)`.
//!
//! This mirrors the [`crate::always_on`] GIFT-Nifty exemption transport exactly.
//! The processor takes the flag as an explicit argument (NOT by reading this
//! global), so its gate logic stays fully unit-testable in isolation — tests
//! pass their own bool and never touch this `OnceLock`.

use std::sync::OnceLock;

/// Boot-set-once Muhurat flag. Production sets this exactly once after the daily
/// calendar is loaded; it is read-only thereafter.
static MUHURAT_ACTIVE: OnceLock<bool> = OnceLock::new();

/// Install the boot-computed Muhurat-session flag. Idempotent: the first call
/// wins; later calls are ignored (boot runs once). Safe to never call —
/// [`current`] then returns `false` (today's behaviour: no Muhurat widening).
// TEST-EXEMPT: covered by tests::current_before_init_is_false_then_reflects_init.
pub fn init_muhurat_session(active: bool) {
    // First call wins; a second call returns Err(rejected) which we
    // intentionally discard (boot runs once). `let _` consumes the
    // #[must_use] Result (a Copy `Result<(), bool>`, so `drop` would no-op).
    let _ = MUHURAT_ACTIVE.set(active);
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
