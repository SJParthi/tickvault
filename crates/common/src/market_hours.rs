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

use std::sync::{Arc, OnceLock};

use crate::constants::{
    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use crate::trading_calendar::TradingCalendar;

/// Global process-wide trading-calendar handle, installed ONCE at boot via
/// [`set_market_calendar_for_session`]. When present, [`is_trading_session_now`]
/// uses it to treat NSE holidays (not just weekends) as market-closed. When
/// absent (unit tests, or the brief boot window before install), the helper
/// falls back to the weekday-only [`is_within_trading_session_ist`] gate.
///
/// Mirrors the `tickvault_core::websocket::connection` `MARKET_CALENDAR`
/// `OnceLock` precedent, but lives in `common` so the api crate's `/feeds`
/// health handler — which has no calendar handle threaded through `AppState`
/// — can ask the trading-day-aware question without a wider refactor.
static SESSION_CALENDAR: OnceLock<Arc<TradingCalendar>> = OnceLock::new();

/// Install the process-wide trading calendar so [`is_trading_session_now`] is
/// holiday-aware. Idempotent: returns `true` on the first install and `false`
/// on every subsequent attempt (the `OnceLock` keeps the first value).
///
/// Called once from boot (`main.rs`). Read-only thereafter — no per-tick cost.
// TEST-EXEMPT: covered by `set_market_calendar_for_session_is_idempotent` + `trading_session_now_uses_installed_calendar_for_holidays` below (the OnceLock can only be installed once per test binary, so these own the install).
pub fn set_market_calendar_for_session(calendar: Arc<TradingCalendar>) -> bool {
    SESSION_CALENDAR.set(calendar).is_ok()
}

/// The trading-day-aware "is the market open RIGHT NOW?" answer used by the
/// `/feeds` live-feed health verdict.
///
/// Returns `true` ONLY when the IST wall-clock is inside `[09:00, 15:30)` AND
/// today is an actual NSE trading day:
/// - If a calendar was installed via [`set_market_calendar_for_session`], today
///   must be a trading day per [`TradingCalendar::is_trading_day_today`] (covers
///   weekends AND NSE weekday holidays).
/// - If no calendar is installed yet, falls back to the weekday-only
///   [`is_within_trading_session_ist`] gate (still closes the dominant ~104
///   weekend days/year; holidays become covered once the calendar installs).
///
/// # Why (feed-health false-RED fix, 2026-06-29)
/// The `/feeds` page previously computed "market open" from the time-of-day-only
/// [`is_within_market_hours_ist`], so on a Saturday/Sunday/holiday inside the
/// 09:00–15:30 clock window it read `true` — the market-closed-idle bypass never
/// fired and a stale `auth_rejected` flag re-surfaced the false "refresh the SSM
/// api-key" Down. Gating on the trading day closes that hole.
///
/// # Complexity
/// O(1): one relaxed atomic load (`OnceLock::get`) + one
/// [`is_within_trading_session_ist`] call (+ one `is_trading_day_today` map
/// lookup when a calendar is installed). Cold path — called per `/feeds`
/// request only.
///
/// # Test override
/// Honours `TEST_FORCE_IN_MARKET_HOURS` (via the inner helpers) so existing
/// market-hours tests keep working.
#[must_use]
#[inline]
pub fn is_trading_session_now() -> bool {
    // If a calendar is installed, the trading-day answer is its real-clock
    // `is_trading_day_today` (covers weekends AND NSE weekday holidays). No
    // calendar → `None`, and the pure combiner falls back to the weekday-only
    // in-session gate.
    let calendar_trading_day_today = SESSION_CALENDAR.get().map(|c| c.is_trading_day_today());
    combine_trading_session(is_within_trading_session_ist(), calendar_trading_day_today)
}

/// Pure combiner for [`is_trading_session_now`] — unit-testable without a clock.
///
/// `in_weekday_session` is the time-of-day + weekday gate
/// ([`is_within_trading_session_ist`]). `calendar_trading_day_today` is
/// `Some(is_trading_day)` when a calendar is installed, `None` otherwise.
///
/// Open ⇔ in the weekday session AND (no calendar OR the calendar says today is
/// a trading day). A calendar that says "today is a holiday" forces closed even
/// inside the weekday clock window — the exact false-RED hole this closes.
#[must_use]
#[inline]
fn combine_trading_session(
    in_weekday_session: bool,
    calendar_trading_day_today: Option<bool>,
) -> bool {
    in_weekday_session && calendar_trading_day_today.unwrap_or(true)
}

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

// ----- Pure seconds-of-day helpers (2026-10-10) ------------------------------
//
// Every wall-clock function below reads the clock ONCE and hands the reading to
// one of these pure helpers. Before this split, each branch of the arithmetic
// (inside vs outside 09:00-15:40 IST, before vs after 09:00, weekday vs
// weekend) ran only when CI happened to run at that time of day, so the
// crate's line coverage depended on the hour: a run during market hours left
// the off-hours lines of `secs_until_next_market_open_ist` uncovered and fell
// to ~99.38% against the 99.4 floor. The helpers take the time as an argument,
// so the tests pin every branch with fixed inputs and coverage no longer moves
// with the clock. Each is O(1): integer arithmetic only, no allocation.

/// IST seconds-of-day in `[0, 86_400)` for a Unix timestamp (seconds, UTC).
///
/// Saturates instead of overflowing near `i64::MAX`, and `rem_euclid` keeps a
/// pre-1970 timestamp in range, so every `i64` input is defined.
#[allow(clippy::cast_possible_truncation)] // APPROVED: rem_euclid against SECONDS_PER_DAY (=86400) fits u32
#[allow(clippy::cast_sign_loss)] // APPROVED: rem_euclid with a positive divisor is never negative
#[must_use]
#[inline]
fn ist_secs_of_day_at(utc_secs: i64) -> u32 {
    utc_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// `true` when an IST seconds-of-day falls in
/// `[TICK_PERSIST_START, TICK_PERSIST_END)`.
#[must_use]
#[inline]
fn in_market_hours_at(sec_of_day: u32) -> bool {
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

/// `true` when a Unix timestamp falls on a Saturday or Sunday in IST.
///
/// Day 0 of the Unix epoch (1970-01-01) was a Thursday, which is index 3 when
/// Monday is 0 (chrono's `TryFrom<u8>` numbering), so the IST weekday index is
/// `(ist_day + 3) mod 7`. `rem_euclid` keeps it in `0..7`, so the `try_from`
/// can never fail; if it somehow did, the day would read as a weekday.
#[must_use]
#[inline]
fn is_ist_weekend_at(utc_secs: i64) -> bool {
    /// Monday-based weekday index of 1970-01-01 (a Thursday).
    const EPOCH_WEEKDAY_FROM_MONDAY: i64 = 3;
    const DAYS_PER_WEEK: i64 = 7;
    let ist_day = utc_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .div_euclid(i64::from(SECONDS_PER_DAY));
    let weekday_from_monday = ist_day
        .saturating_add(EPOCH_WEEKDAY_FROM_MONDAY)
        .rem_euclid(DAYS_PER_WEEK);
    let weekday = u8::try_from(weekday_from_monday)
        .ok()
        .and_then(|index| chrono::Weekday::try_from(index).ok());
    matches!(weekday, Some(chrono::Weekday::Sat | chrono::Weekday::Sun))
}

/// `true` when a Unix timestamp is inside market hours on an IST weekday.
#[must_use]
#[inline]
fn in_trading_session_at(utc_secs: i64) -> bool {
    in_market_hours_at(ist_secs_of_day_at(utc_secs)) && !is_ist_weekend_at(utc_secs)
}

/// Seconds from an IST seconds-of-day to the next IST midnight, in
/// `(0, 86_400]`. An input past the end of the day saturates to 0 rather than
/// wrapping (no caller passes one: every caller reads `ist_secs_of_day_at`).
#[must_use]
#[inline]
fn secs_until_ist_midnight_from(sec_of_day: u32) -> u64 {
    u64::from(SECONDS_PER_DAY.saturating_sub(sec_of_day))
}

/// Seconds from an IST seconds-of-day to the next market open (09:00 IST);
/// `0` inside market hours. Before 09:00 that is today's open; at or after the
/// close it is tomorrow's.
#[must_use]
#[inline]
fn secs_until_market_open_from(sec_of_day: u32) -> u64 {
    if in_market_hours_at(sec_of_day) {
        return 0;
    }
    let open = TICK_PERSIST_START_SECS_OF_DAY_IST;
    let until = if sec_of_day < open {
        open - sec_of_day
    } else {
        SECONDS_PER_DAY
            .saturating_sub(sec_of_day)
            .saturating_add(open)
    };
    u64::from(until)
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
#[inline]
pub fn is_within_market_hours_ist() -> bool {
    if TEST_FORCE_IN_MARKET_HOURS.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    in_market_hours_at(now_ist_secs_of_day())
}

/// Returns `true` ONLY when the IST wall-clock is in
/// `[TICK_PERSIST_START, TICK_PERSIST_END)` AND today is a weekday
/// (Monday–Friday). Saturday and Sunday short-circuit to `false`
/// regardless of time-of-day.
///
/// # Why this exists (Parthiban directive 2026-05-09)
/// On Saturday 2026-05-09 the operator received CRITICAL Telegram pages
/// for "zero live ticks during market hours", "MANDATORY underlying
/// missing", "BOOT DEADLINE MISSED", and an SLO-02 cascade — all
/// because the existing [`is_within_market_hours_ist`] gate only
/// checks the 09:00–15:30 IST time-of-day window, NOT the day of week.
/// NSE doesn't trade on weekends, so the entire panic chain was a
/// false-positive. This helper closes that hole.
///
/// # Scope (intentional)
/// Covers ~104 false-positive boot days/year (every Sat + Sun).
/// NSE-specific weekday holidays (Diwali, Republic Day, etc.) are
/// NOT covered — those require [`crate::trading_calendar::TradingCalendar`]
/// threading. Holiday alerts are 8–12 days/year and can be addressed
/// incrementally.
///
/// # Complexity
/// O(1): one relaxed atomic load + one [`chrono::Utc::now`] + integer
/// math. The time of day and the weekday come from the SAME clock
/// reading (until 2026-10-10 they were two readings, which could
/// straddle midnight). Cold path — called once per watchdog/SLO
/// scheduler tick (≥10s cadence).
///
/// # Test override
/// Honours `TEST_FORCE_IN_MARKET_HOURS` for parity with
/// [`is_within_market_hours_ist`] — when set, returns `true`
/// regardless of weekday or wall-clock.
#[inline]
// TEST-EXEMPT: covered by the tests in this file's `tests` module (`trading_session_returns_bool_without_panic`, `trading_session_force_override_returns_true_when_set`, `trading_session_implies_market_hours`, `in_trading_session_at_*`, `is_ist_weekend_at_*`, `trading_session_helper_contains_weekend_gate`).
pub fn is_within_trading_session_ist() -> bool {
    if TEST_FORCE_IN_MARKET_HOURS.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    in_trading_session_at(chrono::Utc::now().timestamp())
}

/// Returns the number of seconds until the next IST midnight
/// (00:00:00 IST). Returns a value in `(0, 86400]`.
///
/// O(1). Used by the Phase 2 midnight rollover task (29-tf engine
/// plan, L13) to schedule the atomic state transition that clears
/// `volume_delta` baselines, resets `prev_day_close` stamps, and
/// reloads `prev_oi_cache` from yesterday's `candles_1d`.
///
/// # Test override
/// When `TEST_FORCE_IN_MARKET_HOURS` is set, returns 1 — a tiny
/// deterministic value so tests don't sleep for hours.
#[inline]
pub fn secs_until_next_ist_midnight() -> u64 {
    if TEST_FORCE_IN_MARKET_HOURS.load(std::sync::atomic::Ordering::Relaxed) {
        return 1;
    }
    secs_until_ist_midnight_from(now_ist_secs_of_day())
}

/// Returns the current IST seconds-of-day in `[0, 86400)` from
/// `chrono::Utc::now()`. Used by the tick enricher (Phase 2 / 29-tf
/// engine plan) to classify each tick into the trading-day phase
/// without each consumer rolling its own clock + offset arithmetic.
///
/// O(1) — single `Utc::now()` call (vDSO clock_gettime on Linux,
/// ~50ns) + integer math. The vDSO-based clock read is hot-path-safe
/// for our latency budget; we deliberately do NOT cache because the
/// phase boundaries (09:00, 09:15, 15:30, 15:40 IST) can land
/// mid-batch and a stale cache would mis-classify ticks at the
/// boundary.
///
/// # Test override
/// When `TEST_FORCE_IN_MARKET_HOURS` is set, returns 09:30 IST
/// (33000) — a deterministic in-market sec-of-day so tests get
/// repeatable phase classification regardless of wall clock.
#[inline]
pub fn now_ist_secs_of_day() -> u32 {
    if TEST_FORCE_IN_MARKET_HOURS.load(std::sync::atomic::Ordering::Relaxed) {
        // Deterministic 09:30 IST for tests — within OPEN phase.
        return 9 * 3600 + 30 * 60;
    }
    ist_secs_of_day_at(chrono::Utc::now().timestamp())
}

/// Returns the number of seconds until the next `TICK_PERSIST_START_SECS_OF_DAY_IST`
/// (09:00 IST). Returns `0` when currently within market hours.
///
/// Used by WebSocket boot gates that want to defer TCP connect until market
/// open rather than flap against Dhan's pre-market idle-socket resets.
///
/// # Complexity
/// O(1): one clock read plus arithmetic (until 2026-10-10 it read the clock
/// twice, once for the in-hours check and once for the arithmetic).
///
/// # Upper bound
/// At most 24 hours minus the `[09:00, 15:40)` window, i.e. 17h20m = 62,400 s.
///
/// # Test override
/// When `TEST_FORCE_IN_MARKET_HOURS` is set, returns `0` — same semantics as
/// "currently in market hours" (the override pins `now_ist_secs_of_day` to
/// 09:30 IST, which is inside the window).
#[inline]
// TEST-EXEMPT: covered by secs_until_next_open_returns_zero_during_market_hours, secs_until_next_open_bounded_below_one_day, secs_until_next_open_positive_when_off_hours and the fixed-input secs_until_market_open_from_* tests below
pub fn secs_until_next_market_open_ist() -> u64 {
    secs_until_market_open_from(now_ist_secs_of_day())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that touches `TEST_FORCE_IN_MARKET_HOURS`.
    ///
    /// ## The flake this closes (2026-08-14)
    ///
    /// That flag is a PROCESS-GLOBAL atomic, and ten tests in this module set
    /// it. `cargo test` runs them on parallel threads, so one test's
    /// `set(false)` — or its trailing reset — lands in the middle of another
    /// test's forced-`true` window. The victim then reads a value it did not
    /// write and fails an assertion about behaviour that is actually correct.
    ///
    /// Measured before the fix: `set_market_calendar_for_session_is_idempotent_and_gates_holidays`
    /// failed **3 of 5** consecutive module runs, and passed 100% when run
    /// alone. It failed CI on a commit that does not touch this file.
    ///
    /// This is the same shape, and the same fix, as the `TV_API_TOKEN` race
    /// recorded in `merge-gate-lock-2026-07-04.md` §3.2: a shared static
    /// `Mutex` plus a scoped guard that restores the value on drop, poisoning
    /// -safe via `into_inner` so one failing test cannot cascade into every
    /// other test in the module reporting a poisoned lock instead of its own
    /// verdict.
    ///
    /// Holding a guard twice on one thread would DEADLOCK (`std::sync::Mutex`
    /// is not reentrant), which is why the tests take exactly one guard each
    /// and let `Drop` do the reset rather than calling the setter again.
    static FORCE_HOURS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ForceHours(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl ForceHours {
        fn set(value: bool) -> Self {
            let guard = FORCE_HOURS_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            set_test_force_in_market_hours(value);
            Self(guard)
        }
    }

    impl Drop for ForceHours {
        fn drop(&mut self) {
            // Always restore the default, even on an assertion unwind — a test
            // that panics mid-window must not leave the flag forced for
            // whichever test acquires the lock next.
            set_test_force_in_market_hours(false);
        }
    }

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
        let _force_guard = ForceHours::set(true);
        assert!(is_within_market_hours_ist());
    }

    #[test]
    fn test_force_override_resets_cleanly() {
        let _force_guard = ForceHours::set(true);
        set_test_force_in_market_hours(false);
        // After reset, helper falls through to real wall-clock check;
        // we only assert no panic and a bool was produced.
        let _ = is_within_market_hours_ist();
    }

    // ----- is_within_trading_session_ist (Wave-Holiday-Gate 2026-05-09) -----

    /// Smoke test: helper returns a bool without panic.
    #[test]
    fn trading_session_returns_bool_without_panic() {
        let _ = is_within_trading_session_ist();
    }

    /// Test override propagates through the trading-session gate too,
    /// otherwise tests that pin "in market hours" would break the gate.
    #[test]
    fn trading_session_force_override_returns_true_when_set() {
        let _force_guard = ForceHours::set(true);
        assert!(is_within_trading_session_ist());
    }

    /// The trading-session gate MUST always be a subset of the market-hours
    /// gate — if it's currently outside market hours, it's also outside the
    /// trading session, regardless of weekday.
    #[test]
    fn trading_session_implies_market_hours() {
        let _force_guard = ForceHours::set(false);
        if !is_within_market_hours_ist() {
            assert!(
                !is_within_trading_session_ist(),
                "trading session must be false when market hours is false"
            );
        }
    }

    /// Direct test of the weekend-detection logic the gate relies on.
    /// This pins the regression: if someone removes
    /// `Weekday::Sat | Weekday::Sun` from the match expression, this fails.
    #[test]
    fn weekend_weekdays_match_sat_and_sun() {
        use chrono::Weekday;
        assert!(matches!(Weekday::Sat, Weekday::Sat | Weekday::Sun));
        assert!(matches!(Weekday::Sun, Weekday::Sat | Weekday::Sun));
        assert!(!matches!(Weekday::Mon, Weekday::Sat | Weekday::Sun));
        assert!(!matches!(Weekday::Tue, Weekday::Sat | Weekday::Sun));
        assert!(!matches!(Weekday::Wed, Weekday::Sat | Weekday::Sun));
        assert!(!matches!(Weekday::Thu, Weekday::Sat | Weekday::Sun));
        assert!(!matches!(Weekday::Fri, Weekday::Sat | Weekday::Sun));
    }

    /// Source-scan ratchet: the canonical helper MUST contain the weekend
    /// match expression. Fails the build if someone removes the gate.
    #[test]
    fn trading_session_helper_contains_weekend_gate() {
        let src = include_str!("market_hours.rs");
        assert!(
            src.contains("chrono::Weekday::Sat | chrono::Weekday::Sun"),
            "is_within_trading_session_ist must guard against Sat/Sun"
        );
    }

    // ----- is_trading_session_now + combine_trading_session (feed-health
    //        trading-day-aware gate, 2026-06-29) -----------------------------
    // pub-fn-test-guard one-liner: these tests exercise is_trading_session_now + combine_trading_session across every calendar state.

    /// The pure combiner: open ⇔ in the weekday session AND (no calendar OR the
    /// calendar says today is a trading day). This is the deterministic,
    /// clock-free heart of `is_trading_session_now`.
    #[test]
    fn combine_trading_session_truth_table() {
        // No calendar installed → falls back to the weekday-session bool.
        assert!(
            combine_trading_session(true, None),
            "weekday session, no cal → open"
        );
        assert!(
            !combine_trading_session(false, None),
            "outside weekday session, no cal → closed"
        );
        // Calendar present + today IS a trading day → mirrors the session bool.
        assert!(
            combine_trading_session(true, Some(true)),
            "session + trading day → open"
        );
        assert!(
            !combine_trading_session(false, Some(true)),
            "trading day but outside session → closed"
        );
        // Calendar present + today is NOT a trading day (weekend/holiday) →
        // forced CLOSED even inside the weekday clock window. This is the exact
        // hole the fix closes: a holiday at 11:00 IST must read closed so the
        // market-closed-idle bypass fires and a stale auth_rejected stays calm.
        assert!(
            !combine_trading_session(true, Some(false)),
            "holiday/weekend per calendar must force closed even mid-window"
        );
        assert!(!combine_trading_session(false, Some(false)));
    }

    /// `is_trading_session_now()` returns a bool without panic (real clock).
    #[test]
    fn trading_session_now_returns_bool_without_panic() {
        let _ = is_trading_session_now();
    }

    /// When NO calendar is installed, `is_trading_session_now()` must equal the
    /// weekday-only `is_within_trading_session_ist()` fallback — never a worse
    /// false-RED than today's weekday gate. (This test binary may or may not
    /// have had a calendar installed by `set_market_calendar_for_session_*`; we
    /// only assert the fallback identity holds when the calendar is absent.)
    #[test]
    fn trading_session_now_falls_back_to_weekday_gate_when_no_calendar() {
        let _force_guard = ForceHours::set(false);
        if SESSION_CALENDAR.get().is_none() {
            assert_eq!(
                is_trading_session_now(),
                is_within_trading_session_ist(),
                "no calendar → must mirror the weekday-only gate"
            );
        }
    }

    /// Install the session calendar ONCE (this test owns the `OnceLock` per
    /// binary) and prove (a) the install is idempotent, and (b) a calendar that
    /// reports today as a non-trading day forces `is_trading_session_now` closed
    /// even when the in-hours weekday gate is forced true. This is the
    /// holiday/weekend false-RED fix end-to-end.
    #[test]
    fn set_market_calendar_for_session_is_idempotent_and_gates_holidays() {
        use crate::config::TradingConfig;
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        let cal = std::sync::Arc::new(TradingCalendar::from_config(&cfg).unwrap());

        let first = set_market_calendar_for_session(cal.clone());
        let second = set_market_calendar_for_session(cal.clone());
        // Whichever call won first must be the only Ok — idempotent.
        assert!(
            first ^ second || (!first && !second),
            "set_market_calendar_for_session must install at most once"
        );

        // With a calendar installed, force the in-hours weekday gate true. The
        // combined answer must equal the calendar's is_trading_day_today() — so
        // on a weekend/holiday (calendar says false) it is CLOSED despite the
        // forced in-hours flag, and on a real weekday it is OPEN.
        let _force_guard = ForceHours::set(true);
        let expected = SESSION_CALENDAR
            .get()
            .map(|c| c.is_trading_day_today())
            .unwrap_or(true);
        assert_eq!(
            is_trading_session_now(),
            expected,
            "with a calendar installed, the session gate must follow is_trading_day_today"
        );
    }

    /// Guard: the window bounds come from common constants, not hardcoded
    /// literals. If NSE ever changes to 08:45-15:45, only one line changes.
    #[test]
    fn window_bounds_come_from_tick_persist_constants() {
        assert_eq!(TICK_PERSIST_START_SECS_OF_DAY_IST, 9 * 3600);
        // 2026-08-07: NSE moved the F&O close to 15:40 on 2026-08-03 for the
        // new Closing Auction Session — the "only one line changes" promise
        // in this guard's doc comment held: the canonical constant moved and
        // every dependent followed via const-assert.
        assert_eq!(TICK_PERSIST_END_SECS_OF_DAY_IST, 15 * 3600 + 40 * 60);
    }

    // ----- secs_until_next_market_open_ist ----------------------------------

    #[test]
    fn secs_until_next_open_returns_zero_during_market_hours() {
        let _force_guard = ForceHours::set(true);
        assert_eq!(secs_until_next_market_open_ist(), 0);
    }

    /// Upper-bound sanity: 24h == 86,400s. The answer must be less than
    /// 24h because there is at most one full day between two 09:00 IST
    /// market opens.
    #[test]
    fn secs_until_next_open_bounded_below_one_day() {
        let secs = secs_until_next_market_open_ist();
        assert!(
            secs < u64::from(SECONDS_PER_DAY),
            "secs_until_next_market_open_ist() = {secs} must be < 24h ({})",
            SECONDS_PER_DAY
        );
    }

    /// If NOT in market hours, the helper MUST return a positive number of
    /// seconds; returning 0 off-hours would make the boot-time defer loop
    /// a no-op and re-introduce the flap.
    #[test]
    fn secs_until_next_open_positive_when_off_hours() {
        // Force off-hours by clearing the override (default state).
        let _force_guard = ForceHours::set(false);
        if !is_within_market_hours_ist() {
            assert!(
                secs_until_next_market_open_ist() > 0,
                "off-hours must return > 0 secs"
            );
        }
        // If the real wall-clock happens to be inside [09:00, 15:30) IST
        // while CI runs this test, we can't deterministically assert > 0
        // — but `secs_until_next_open_returns_zero_during_market_hours`
        // covers that direction.
    }

    /// Phase 2 ratchet: `now_ist_secs_of_day` returns a value in
    /// `[0, 86400)`. The wall-clock reading itself is non-deterministic
    /// in CI but the bound is invariant.
    #[test]
    fn test_now_ist_secs_of_day_is_bounded() {
        let s = now_ist_secs_of_day();
        assert!(s < 86400, "secs-of-day must be < 86400, got {s}");
    }

    /// Phase 2 ratchet: when `TEST_FORCE_IN_MARKET_HOURS` is set, the
    /// helper returns the deterministic 09:30 IST value so tick-enricher
    /// integration tests see a stable OPEN-phase classification
    /// regardless of when CI runs.
    #[test]
    fn test_now_ist_secs_of_day_test_override_returns_open_phase() {
        let _force_guard = ForceHours::set(true);
        let s = now_ist_secs_of_day();
        assert_eq!(s, 9 * 3600 + 30 * 60, "test override must return 09:30 IST");
    }

    /// Phase 2.7 ratchet (hostile bug-hunt CRITICAL C1 fix): the
    /// midnight-rollover scheduler MUST return a positive value (the
    /// rollover tick fires AFTER the boundary, never before) and
    /// MUST NOT exceed 24h.
    #[test]
    fn test_secs_until_next_ist_midnight_is_bounded() {
        let s = secs_until_next_ist_midnight();
        assert!(s > 0, "secs_until_next_ist_midnight must be positive");
        assert!(
            s <= 86400,
            "secs_until_next_ist_midnight must be <= 24h, got {s}"
        );
    }

    /// Phase 2.7 ratchet: test override returns 1 so the rollover
    /// scheduler test exercises the loop without a 24h sleep.
    #[test]
    fn test_secs_until_next_ist_midnight_test_override() {
        let _force_guard = ForceHours::set(true);
        let s = secs_until_next_ist_midnight();
        assert_eq!(s, 1, "test override must return 1s");
    }

    // ----- Pure seconds-of-day helpers (2026-10-10) --------------------------
    // Fixed inputs, so every branch is covered whatever time CI runs. Before
    // these, the crate's coverage moved with the hour of the run.

    /// Unix seconds (UTC) for an IST wall-clock instant.
    fn ist_instant(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, second)
            .unwrap()
            .and_utc()
            .timestamp()
            - i64::from(IST_UTC_OFFSET_SECONDS)
    }

    /// Reference IST weekday from chrono, for the equivalence checks.
    fn chrono_ist_weekday(utc_secs: i64) -> chrono::Weekday {
        use chrono::Datelike;
        (chrono::DateTime::from_timestamp(utc_secs, 0).unwrap()
            + chrono::TimeDelta::seconds(i64::from(IST_UTC_OFFSET_SECONDS)))
        .weekday()
    }

    #[test]
    fn ist_secs_of_day_at_fixed_instants() {
        // The Unix epoch is 05:30:00 IST.
        assert_eq!(ist_secs_of_day_at(0), 5 * 3600 + 30 * 60);
        // One second before the epoch stays in range (rem_euclid, not %).
        assert_eq!(ist_secs_of_day_at(-1), 5 * 3600 + 30 * 60 - 1);
        assert_eq!(
            ist_secs_of_day_at(ist_instant(2026, 10, 9, 9, 0, 0)),
            32_400
        );
        assert_eq!(
            ist_secs_of_day_at(ist_instant(2026, 10, 9, 15, 40, 0)),
            56_400
        );
        assert_eq!(ist_secs_of_day_at(ist_instant(2026, 10, 9, 0, 0, 0)), 0);
        assert_eq!(
            ist_secs_of_day_at(ist_instant(2026, 10, 9, 23, 59, 59)),
            86_399
        );
    }

    #[test]
    fn ist_secs_of_day_at_extreme_inputs_stay_in_range() {
        for utc_secs in [
            i64::MIN,
            i64::MIN + 1,
            -86_400,
            86_400,
            i64::MAX - 1,
            i64::MAX,
        ] {
            assert!(
                ist_secs_of_day_at(utc_secs) < SECONDS_PER_DAY,
                "out of range for {utc_secs}"
            );
        }
    }

    #[test]
    fn in_market_hours_at_window_edges() {
        assert!(!in_market_hours_at(0));
        assert!(!in_market_hours_at(TICK_PERSIST_START_SECS_OF_DAY_IST - 1));
        assert!(in_market_hours_at(TICK_PERSIST_START_SECS_OF_DAY_IST));
        assert!(in_market_hours_at(TICK_PERSIST_END_SECS_OF_DAY_IST - 1));
        assert!(!in_market_hours_at(TICK_PERSIST_END_SECS_OF_DAY_IST));
        assert!(!in_market_hours_at(SECONDS_PER_DAY - 1));
    }

    #[test]
    fn is_ist_weekend_at_fixed_days() {
        // 2026-10-09 Friday, 10 Saturday, 11 Sunday, 12 Monday.
        assert!(!is_ist_weekend_at(ist_instant(2026, 10, 9, 12, 0, 0)));
        assert!(is_ist_weekend_at(ist_instant(2026, 10, 10, 12, 0, 0)));
        assert!(is_ist_weekend_at(ist_instant(2026, 10, 11, 12, 0, 0)));
        assert!(!is_ist_weekend_at(ist_instant(2026, 10, 12, 12, 0, 0)));
        // The weekday turns over at IST midnight, not UTC midnight: Friday
        // 18:30 UTC is already Saturday 00:00 IST.
        assert!(!is_ist_weekend_at(ist_instant(2026, 10, 9, 23, 59, 59)));
        assert!(is_ist_weekend_at(ist_instant(2026, 10, 10, 0, 0, 0)));
        assert!(is_ist_weekend_at(ist_instant(2026, 10, 11, 23, 59, 59)));
        assert!(!is_ist_weekend_at(ist_instant(2026, 10, 12, 0, 0, 0)));
        // 1970-01-01 was a Thursday; 1969-12-28 a Sunday (pre-epoch input).
        assert!(!is_ist_weekend_at(0));
        assert!(is_ist_weekend_at(ist_instant(1969, 12, 28, 12, 0, 0)));
    }

    #[test]
    fn is_ist_weekend_at_extreme_inputs_do_not_panic() {
        for utc_secs in [i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX] {
            let _ = is_ist_weekend_at(utc_secs);
        }
    }

    #[test]
    fn in_trading_session_at_fixed_instants() {
        // Friday inside the window.
        assert!(in_trading_session_at(ist_instant(2026, 10, 9, 9, 0, 0)));
        assert!(in_trading_session_at(ist_instant(2026, 10, 9, 15, 39, 59)));
        // Friday outside the window.
        assert!(!in_trading_session_at(ist_instant(2026, 10, 9, 8, 59, 59)));
        assert!(!in_trading_session_at(ist_instant(2026, 10, 9, 15, 40, 0)));
        // Saturday and Sunday inside the clock window stay closed.
        assert!(!in_trading_session_at(ist_instant(2026, 10, 10, 11, 0, 0)));
        assert!(!in_trading_session_at(ist_instant(2026, 10, 11, 11, 0, 0)));
        // Monday reopens.
        assert!(in_trading_session_at(ist_instant(2026, 10, 12, 11, 0, 0)));
    }

    #[test]
    fn secs_until_ist_midnight_from_fixed_values() {
        assert_eq!(secs_until_ist_midnight_from(0), 86_400);
        assert_eq!(secs_until_ist_midnight_from(1), 86_399);
        assert_eq!(secs_until_ist_midnight_from(33_000), 53_400);
        assert_eq!(secs_until_ist_midnight_from(86_399), 1);
        // Out-of-range input saturates instead of wrapping.
        assert_eq!(secs_until_ist_midnight_from(90_000), 0);
    }

    #[test]
    fn secs_until_market_open_from_fixed_values() {
        let open = u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST);
        let close = TICK_PERSIST_END_SECS_OF_DAY_IST;
        let day = u64::from(SECONDS_PER_DAY);
        // Before the open: today's 09:00.
        assert_eq!(secs_until_market_open_from(0), open);
        assert_eq!(
            secs_until_market_open_from(TICK_PERSIST_START_SECS_OF_DAY_IST - 1),
            1
        );
        // Inside the window: zero.
        assert_eq!(
            secs_until_market_open_from(TICK_PERSIST_START_SECS_OF_DAY_IST),
            0
        );
        assert_eq!(secs_until_market_open_from(close - 1), 0);
        // At or after the close: tomorrow's 09:00. The longest wait is from
        // the close itself: 86,400 - 56,400 + 32,400 = 62,400 s.
        assert_eq!(
            secs_until_market_open_from(close),
            day - u64::from(close) + open
        );
        assert_eq!(secs_until_market_open_from(close), 62_400);
        assert_eq!(secs_until_market_open_from(SECONDS_PER_DAY - 1), open + 1);
    }

    proptest::proptest! {
        /// The integer weekday matches chrono's for any instant between
        /// roughly 1653 and 2286 AD.
        #[test]
        fn is_ist_weekend_at_matches_chrono(utc_secs in -10_000_000_000_i64..10_000_000_000_i64) {
            let weekend = matches!(
                chrono_ist_weekday(utc_secs),
                chrono::Weekday::Sat | chrono::Weekday::Sun
            );
            proptest::prop_assert_eq!(is_ist_weekend_at(utc_secs), weekend);
        }

        /// Seconds-of-day matches chrono's IST wall clock.
        #[test]
        fn ist_secs_of_day_at_matches_chrono(utc_secs in -10_000_000_000_i64..10_000_000_000_i64) {
            use chrono::Timelike;
            let ist = chrono::DateTime::from_timestamp(utc_secs, 0).unwrap()
                + chrono::TimeDelta::seconds(i64::from(IST_UTC_OFFSET_SECONDS));
            proptest::prop_assert_eq!(ist_secs_of_day_at(utc_secs), ist.num_seconds_from_midnight());
        }

        /// Waiting the returned seconds lands exactly on 09:00 IST, and the
        /// wait never reaches a full day.
        #[test]
        fn secs_until_market_open_from_lands_on_the_open(sec_of_day in 0_u32..SECONDS_PER_DAY) {
            let wait = secs_until_market_open_from(sec_of_day);
            proptest::prop_assert!(wait < u64::from(SECONDS_PER_DAY));
            if in_market_hours_at(sec_of_day) {
                proptest::prop_assert_eq!(wait, 0);
            } else {
                proptest::prop_assert!(wait > 0);
                let landing = (u64::from(sec_of_day) + wait) % u64::from(SECONDS_PER_DAY);
                proptest::prop_assert_eq!(landing, u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST));
            }
        }

        /// Waiting the returned seconds lands exactly on IST midnight.
        #[test]
        fn secs_until_ist_midnight_from_lands_on_midnight(sec_of_day in 0_u32..SECONDS_PER_DAY) {
            let wait = secs_until_ist_midnight_from(sec_of_day);
            proptest::prop_assert!(wait > 0 && wait <= u64::from(SECONDS_PER_DAY));
            proptest::prop_assert_eq!((u64::from(sec_of_day) + wait) % u64::from(SECONDS_PER_DAY), 0);
        }
    }
}
