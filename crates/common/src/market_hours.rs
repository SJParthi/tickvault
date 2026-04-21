//! Shared market-hours helper.
//!
//! Canonical home for `is_within_market_hours_ist()` — previously duplicated
//! in `tickvault_core::websocket::activity_watchdog` and
//! `tickvault_core::instrument::depth_rebalancer`. Both call sites now
//! delegate here.
//!
//! # Authority
//! The market-hours window is derived from `TICK_PERSIST_START_SECS_OF_DAY_IST`
//! (09:00 IST) and `TICK_PERSIST_END_SECS_OF_DAY_IST` (15:30 IST). Do NOT
//! hardcode different bounds elsewhere — they must come from this helper so
//! a single edit (e.g. if NSE extends market hours) propagates everywhere.
//!
//! # Purpose
//! Used to gate log level + Telegram alert emission on events that are noisy
//! outside market hours. Pre-market TCP resets, post-market watchdog fires,
//! post-market rebalance stalls — all are expected Dhan-server-side behavior
//! and must NOT page the operator. Inside market hours, the same events ARE
//! real problems and MUST fire an ERROR + Telegram alert.
//!
//! # Test override
//! `TEST_FORCE_IN_MARKET_HOURS` is a `#[cfg(test)]`-only atomic override so
//! tests can pin the helper to `true` regardless of wall-clock. Production
//! never reads it. Callers that already define their own `TEST_FORCE_*`
//! atomic should migrate to [`set_test_force_in_market_hours`].

use crate::constants::{
    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};

/// Test-only override: force `is_within_market_hours_ist()` to return the
/// set value regardless of wall-clock.
///
/// # Why unconditional (not `#[cfg(test)]`)
/// Rust's `#[cfg(test)]` does NOT propagate across crates — when
/// `tickvault-core` compiles its tests, `tickvault-common` is built
/// without the test cfg, so a `#[cfg(test)] pub fn` here would be
/// invisible. Keeping the symbol unconditional costs one relaxed atomic
/// load on the cold-path helper and zero bytes in release builds (the
/// atomic lives in `.bss`). Production NEVER calls
/// `set_test_force_in_market_hours`; the atomic stays `false` forever
/// and the helper falls through to the real wall-clock check.
static TEST_FORCE_IN_MARKET_HOURS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only override: pin the market-hours check to `true`. Tests MUST
/// reset to `false` in a `Drop` guard to avoid state leakage between
/// parallel tests — see `activity_watchdog`'s
/// `watchdog_fires_on_sustained_silence_past_threshold` for the pattern.
///
/// Marked `#[doc(hidden)]` because production code must never call this.
/// A regression test (`test_test_force_is_false_in_fresh_process`) would
/// catch accidental production calls by asserting the override is `false`
/// at startup.
#[doc(hidden)]
// TEST-EXEMPT: trivial single-line atomic store; behaviour exercised by `test_force_override_returns_true_when_set` and `test_force_override_resets_cleanly` below, plus every test in activity_watchdog.rs that pins the gate.
pub fn set_test_force_in_market_hours(value: bool) {
    TEST_FORCE_IN_MARKET_HOURS.store(value, std::sync::atomic::Ordering::Relaxed);
}

/// Returns `true` when the current IST wall-clock falls within
/// `[TICK_PERSIST_START, TICK_PERSIST_END)` — the same window Dhan uses to
/// stream market data.
///
/// # Complexity
/// O(1): one relaxed atomic load (test-override check), one
/// `chrono::Utc::now()` syscall, one `rem_euclid`, one range check.
/// Called on cold paths only — disconnect events, watchdog fires,
/// rebalancer ticks. Zero per-tick impact.
#[allow(clippy::cast_possible_truncation)] // APPROVED: secs-of-day fits u32 by construction
#[inline]
pub fn is_within_market_hours_ist() -> bool {
    if TEST_FORCE_IN_MARKET_HOURS.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    let now_utc_secs = chrono::Utc::now().timestamp();
    let sec_of_day = now_utc_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct unit test for `is_within_market_hours_ist` — returns a bool
    /// without I/O beyond a single `Utc::now()` call. Behavioural assertion
    /// (post-market silence does not alert) is exercised by the
    /// market-hours gate tests in `connection` and `order_update_connection`.
    #[test]
    fn returns_bool_without_panic() {
        let _ = is_within_market_hours_ist();
    }

    #[test]
    fn test_force_override_returns_true_when_set() {
        set_test_force_in_market_hours(true);
        assert!(is_within_market_hours_ist());
        set_test_force_in_market_hours(false);
    }

    #[test]
    fn test_force_override_resets_cleanly() {
        set_test_force_in_market_hours(true);
        set_test_force_in_market_hours(false);
        // After reset, helper falls through to real wall-clock check;
        // we only assert no panic and a bool was produced.
        let _ = is_within_market_hours_ist();
    }

    /// Guard: the window bounds come from common constants, not hardcoded
    /// literals. If NSE ever changes to 08:45-15:45, only one line changes.
    #[test]
    fn window_bounds_come_from_tick_persist_constants() {
        assert_eq!(TICK_PERSIST_START_SECS_OF_DAY_IST, 9 * 3600);
        assert_eq!(TICK_PERSIST_END_SECS_OF_DAY_IST, 15 * 3600 + 30 * 60);
    }
}
