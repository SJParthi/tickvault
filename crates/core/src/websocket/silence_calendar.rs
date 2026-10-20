//! The trading-day half of the frame-silence gate (2026-10-20).
//!
//! # What went wrong
//!
//! On 2026-10-20 (Dussehra, an NSE holiday in `config/base.toml`) the box
//! started, the lane dialled every socket, and the frame-silence watchdog
//! redialled them 569 times between 09:15 and 15:30 IST. The market was shut,
//! so no socket carried data, and every silence threshold was crossed again
//! and again. [`super::pool_supervisor::FrameSilenceGate::ContinuousSessionIst`]
//! checked the time of day only, never the calendar.
//!
//! # What this module does
//!
//! [`session_secs_of_day_now`] is the IST seconds-of-day the gate reads. On a
//! day the installed [`TradingCalendar`] says is not a trading day (weekend or
//! NSE holiday) it returns [`NON_TRADING_DAY_SECS_OF_DAY_IST`] (midnight),
//! which is outside every frame-silence window, so the data-silence redials
//! (the 300 s fallback, the 60 s / 90 s sibling-confirmed fast paths and the
//! 900 s depth-200 backstop) and the overflow probe window all stay shut for
//! the whole day. The silence clock is HELD, as it is pre-open.
//!
//! NOT touched: the idle (ping) watchdog, the reaction to a close code, and
//! every other redial path; they do not read this gate.
//!
//! # Fail open
//!
//! With no calendar installed (unit tests, or a lane that never installed
//! one) every day counts as a trading day, i.e. the behaviour before this
//! module. Suppressing a silence redial on a real trading day would hide a
//! dead subscription; a redial on a holiday is only noise. The app installs
//! the lane's calendar with [`install_silence_calendar`] before any socket is
//! dialled.
//!
//! # Complexity
//!
//! O(1) per call, no allocation: two wall-clock reads (the unix time for the
//! IST day, and `now_ist_secs_of_day`, which keeps its test override), then
//! one atomic load and compare against the verdict cached for that IST day. Only the first
//! call of a new IST day asks the calendar (one weekday check and one hash
//! probe) and stores the new verdict with one swap. Called on the 1 s idle
//! poll of each connection task, never per frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tracing::info;

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY};
use tickvault_common::trading_calendar::TradingCalendar;

/// The seconds-of-day the gate reads on a non-trading day: midnight, outside
/// the continuous session and the confirmed window alike.
pub const NON_TRADING_DAY_SECS_OF_DAY_IST: u32 = 0;

/// The calendar the gate consults, installed once by the app.
static CALENDAR: OnceLock<Arc<TradingCalendar>> = OnceLock::new();

/// The verdict for the most recent IST day asked about, packed as
/// `((ist_day + 1) << 1) | trading`. `0` means nothing cached yet.
static DAY_VERDICT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Per-test calendar, so a test can model a holiday without touching
    /// the process-wide cell other tests share.
    static TEST_CALENDAR: std::cell::RefCell<Option<Arc<TradingCalendar>>> =
        const { std::cell::RefCell::new(None) };
    /// Per-test verdict cache, paired with [`TEST_CALENDAR`].
    static TEST_VERDICT: AtomicU64 = const { AtomicU64::new(0) };
}

/// Installs the trading calendar the frame-silence gate consults. The first
/// install wins; returns `false` when one was already installed.
pub fn install_silence_calendar(calendar: Arc<TradingCalendar>) -> bool {
    install_calendar_in(&CALENDAR, calendar)
}

/// The pure half of [`install_silence_calendar`], so a test never sets the
/// process-wide cell (any installed calendar makes weekends shut, which
/// would change unrelated tests that pin the session clock).
fn install_calendar_in(
    cell: &OnceLock<Arc<TradingCalendar>>,
    calendar: Arc<TradingCalendar>,
) -> bool {
    cell.set(calendar).is_ok()
}

/// The IST seconds-of-day the frame-silence gate reads now: the wall clock
/// on a trading day, [`NON_TRADING_DAY_SECS_OF_DAY_IST`] on any other day.
#[must_use]
pub fn session_secs_of_day_now() -> u32 {
    let unix_secs = chrono::Utc::now().timestamp();
    let secs_of_day = tickvault_common::market_hours::now_ist_secs_of_day();
    #[cfg(test)]
    if let Some(secs) = TEST_CALENDAR.with(|cell| {
        cell.borrow().as_ref().map(|calendar| {
            TEST_VERDICT
                .with(|cache| session_secs_of_day_at(unix_secs, secs_of_day, Some(calendar), cache))
        })
    }) {
        return secs;
    }
    session_secs_of_day_at(
        unix_secs,
        secs_of_day,
        CALENDAR.get().map(AsRef::as_ref),
        &DAY_VERDICT,
    )
}

/// The pure half of [`session_secs_of_day_now`].
fn session_secs_of_day_at(
    unix_secs: i64,
    secs_of_day: u32,
    calendar: Option<&TradingCalendar>,
    cache: &AtomicU64,
) -> u32 {
    if is_trading_day_cached(unix_secs, calendar, cache) {
        secs_of_day
    } else {
        NON_TRADING_DAY_SECS_OF_DAY_IST
    }
}

/// Whether the IST day holding `unix_secs` is a trading day, read from
/// `cache` when it already holds that day. Fails open (true) with no
/// calendar or an unrepresentable date.
fn is_trading_day_cached(
    unix_secs: i64,
    calendar: Option<&TradingCalendar>,
    cache: &AtomicU64,
) -> bool {
    let Some(calendar) = calendar else {
        return true;
    };
    let ist_day = unix_secs
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .div_euclid(i64::from(SECONDS_PER_DAY));
    let Ok(day) = u64::try_from(ist_day) else {
        return true;
    };
    let key = day.saturating_add(1);
    let stored = cache.load(Ordering::Acquire);
    if stored >> 1 == key {
        return stored & 1 == 1;
    }
    // The IST day number counts days from 1970-01-01, so UTC midnight of
    // that day number carries the same calendar date.
    let date =
        chrono::DateTime::from_timestamp(ist_day.saturating_mul(i64::from(SECONDS_PER_DAY)), 0)
            .map(|at| at.date_naive());
    let Some(date) = date else {
        return true;
    };
    let trading = calendar.is_trading_day(date);
    let packed = (key << 1) | u64::from(trading);
    if cache.swap(packed, Ordering::AcqRel) != packed && !trading {
        info!(
            date = %date,
            "today is not an NSE trading day: frame-silence redials and the overflow probe \
             window stay shut all day (the ping watchdog and close codes still act)"
        );
    }
    trading
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::pool_budget::{ConnectionSlot, DhanAccount, DhanEndpointType};
    use crate::websocket::pool_supervisor::{
        ConnEvent, ConnectionSupervisor, FrameSilenceGate, SupervisorAction,
    };
    use crate::websocket::types::DisconnectCode;
    use std::time::{Duration, Instant};
    use tickvault_common::config::{NseHolidayEntry, TradingConfig};

    /// 2026-10-20 (Dussehra), a Tuesday.
    const HOLIDAY: &str = "2026-10-20";
    /// 2026-10-20 10:00:00 IST as unix seconds.
    const HOLIDAY_10AM_UNIX: i64 = 1_792_470_600;
    /// 2026-10-21 10:00:00 IST (a Wednesday, a trading day).
    const NEXT_DAY_10AM_UNIX: i64 = HOLIDAY_10AM_UNIX + 86_400;
    /// 2026-10-24 10:00:00 IST (a Saturday).
    const SATURDAY_10AM_UNIX: i64 = HOLIDAY_10AM_UNIX + 4 * 86_400;
    /// 10:00:00 IST as seconds of day.
    const TEN_AM: u32 = 10 * 3600;

    fn calendar_with_holidays(dates: &[&str]) -> TradingCalendar {
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: dates
                .iter()
                .map(|d| NseHolidayEntry {
                    date: (*d).to_string(),
                    name: "Holiday".to_string(),
                })
                .collect(),
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        TradingCalendar::from_config(&cfg).expect("test calendar builds")
    }

    fn today_ist() -> String {
        chrono::Utc::now()
            .with_timezone(&tickvault_common::trading_calendar::ist_offset())
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }

    /// Installs a per-thread calendar for the life of the guard.
    struct TestCalendarGuard;
    impl TestCalendarGuard {
        fn install(calendar: TradingCalendar) -> Self {
            TEST_CALENDAR.with(|c| *c.borrow_mut() = Some(Arc::new(calendar)));
            TEST_VERDICT.with(|v| v.store(0, Ordering::Relaxed));
            Self
        }
    }
    impl Drop for TestCalendarGuard {
        fn drop(&mut self) {
            TEST_CALENDAR.with(|c| *c.borrow_mut() = None);
            TEST_VERDICT.with(|v| v.store(0, Ordering::Relaxed));
        }
    }

    fn live_supervisor(
        endpoint: DhanEndpointType,
        global_index: u8,
    ) -> (ConnectionSupervisor, Instant) {
        let now = Instant::now();
        let mut s = ConnectionSupervisor::new(
            ConnectionSlot {
                account: DhanAccount::Primary,
                endpoint,
                pool_index: 0,
                global_index,
            },
            now,
        );
        s.set_frame_silence_gate(FrameSilenceGate::ContinuousSessionIst);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);
        (s, now)
    }

    #[test]
    fn test_holiday_closes_both_silence_windows_at_ten_am() {
        let calendar = calendar_with_holidays(&[HOLIDAY]);
        let cache = AtomicU64::new(0);
        let secs = session_secs_of_day_at(HOLIDAY_10AM_UNIX, TEN_AM, Some(&calendar), &cache);
        assert_eq!(secs, NON_TRADING_DAY_SECS_OF_DAY_IST);
        let gate = FrameSilenceGate::ContinuousSessionIst;
        assert!(!gate.is_open_at(secs), "no silence redial on a holiday");
        assert!(
            !gate.confirmed_window_open_at(secs),
            "no fast path on a holiday"
        );
    }

    #[test]
    fn test_trading_day_keeps_the_wall_clock_and_the_windows_open() {
        let calendar = calendar_with_holidays(&[HOLIDAY]);
        let cache = AtomicU64::new(0);
        let secs = session_secs_of_day_at(NEXT_DAY_10AM_UNIX, TEN_AM, Some(&calendar), &cache);
        assert_eq!(secs, TEN_AM);
        let gate = FrameSilenceGate::ContinuousSessionIst;
        assert!(gate.is_open_at(secs));
        assert!(gate.confirmed_window_open_at(secs));
    }

    #[test]
    fn test_weekend_is_not_a_trading_day() {
        let calendar = calendar_with_holidays(&[]);
        let cache = AtomicU64::new(0);
        assert_eq!(
            session_secs_of_day_at(SATURDAY_10AM_UNIX, TEN_AM, Some(&calendar), &cache),
            NON_TRADING_DAY_SECS_OF_DAY_IST
        );
    }

    #[test]
    fn test_no_calendar_fails_open() {
        let cache = AtomicU64::new(0);
        assert_eq!(
            session_secs_of_day_at(HOLIDAY_10AM_UNIX, TEN_AM, None, &cache),
            TEN_AM,
            "with no calendar every day is a trading day, as before"
        );
        assert_eq!(
            cache.load(Ordering::Relaxed),
            0,
            "nothing is cached without a calendar"
        );
    }

    #[test]
    fn test_verdict_is_cached_per_ist_day_and_refreshed_on_a_new_day() {
        let holiday = calendar_with_holidays(&[HOLIDAY]);
        let cache = AtomicU64::new(0);
        assert!(!is_trading_day_cached(
            HOLIDAY_10AM_UNIX,
            Some(&holiday),
            &cache
        ));
        let cached = cache.load(Ordering::Relaxed);
        assert_ne!(cached, 0);
        // Same IST day, a calendar that disagrees: the cached verdict wins,
        // which proves the second call never asked the calendar.
        let no_holidays = calendar_with_holidays(&[]);
        assert!(!is_trading_day_cached(
            HOLIDAY_10AM_UNIX + 3600,
            Some(&no_holidays),
            &cache
        ));
        assert_eq!(cache.load(Ordering::Relaxed), cached);
        // A new IST day is asked afresh.
        assert!(is_trading_day_cached(
            NEXT_DAY_10AM_UNIX,
            Some(&holiday),
            &cache
        ));
        assert_ne!(cache.load(Ordering::Relaxed), cached);
    }

    #[test]
    fn test_ist_day_boundary_is_ist_midnight_not_utc() {
        let calendar = calendar_with_holidays(&[HOLIDAY]);
        // 2026-10-19 23:59:59 IST: still the trading day before the holiday.
        let before = HOLIDAY_10AM_UNIX - 10 * 3600 - 1;
        assert!(is_trading_day_cached(
            before,
            Some(&calendar),
            &AtomicU64::new(0)
        ));
        // 2026-10-20 00:00:00 IST: the holiday.
        assert!(!is_trading_day_cached(
            before + 1,
            Some(&calendar),
            &AtomicU64::new(0)
        ));
    }

    #[test]
    fn test_install_silence_calendar_first_install_wins() {
        // Through the pure half on a local cell: the process-wide one is
        // never set by a test.
        let cell = OnceLock::new();
        assert!(install_calendar_in(
            &cell,
            Arc::new(calendar_with_holidays(&[HOLIDAY]))
        ));
        assert!(!install_calendar_in(
            &cell,
            Arc::new(calendar_with_holidays(&[]))
        ));
        let installed = cell.get().map(AsRef::as_ref);
        assert_eq!(
            session_secs_of_day_at(HOLIDAY_10AM_UNIX, TEN_AM, installed, &AtomicU64::new(0)),
            NON_TRADING_DAY_SECS_OF_DAY_IST,
            "the first calendar stays the one consulted"
        );
    }

    #[test]
    fn test_session_secs_of_day_now_reads_the_test_calendar() {
        let _guard = TestCalendarGuard::install(calendar_with_holidays(&[today_ist().as_str()]));
        assert_eq!(session_secs_of_day_now(), NON_TRADING_DAY_SECS_OF_DAY_IST);
    }

    /// The live finding: on a holiday a main-feed socket that keeps receiving
    /// pings but no data must not be redialled for data silence, at any hour.
    /// Today is marked a holiday, so the result does not depend on the time
    /// the test runs.
    #[test]
    fn test_holiday_suppresses_the_silence_redial_but_not_the_ping_watchdog() {
        use crate::websocket::idle_watchdog::IDLE_RECONNECT_TIMEOUT_SECS;
        let _guard = TestCalendarGuard::install(calendar_with_holidays(&[today_ist().as_str()]));
        for (endpoint, global_index) in [
            (DhanEndpointType::MainFeed, 0_u8),
            (DhanEndpointType::Depth200, 10_u8),
        ] {
            let (mut s, now) = live_supervisor(endpoint, global_index);
            // 20 minutes of pings and no data: past the 300 s fallback and
            // the 900 s depth-200 backstop.
            let mut at = now;
            for _ in 0..60 {
                at += Duration::from_secs(20);
                let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
                assert_eq!(
                    s.poll(at + Duration::from_secs(1)),
                    SupervisorAction::Continue,
                    "no data-silence redial on a holiday ({endpoint:?})"
                );
            }
            assert_eq!(s.reconnects(), 0);
            // The ping watchdog still bites when the pings stop.
            assert!(
                matches!(
                    s.poll(at + Duration::from_secs(IDLE_RECONNECT_TIMEOUT_SECS)),
                    SupervisorAction::SleepThenDial { .. }
                ),
                "a transport with no pings is still redialled on a holiday"
            );
        }
    }

    #[test]
    fn test_holiday_leaves_close_code_handling_unchanged() {
        let _guard = TestCalendarGuard::install(calendar_with_holidays(&[today_ist().as_str()]));
        let (mut s, now) = live_supervisor(DhanEndpointType::MainFeed, 1);
        assert!(
            matches!(
                s.on_event(
                    ConnEvent::Disconnected {
                        code: Some(DisconnectCode::AccessTokenInvalid),
                    },
                    now,
                ),
                SupervisorAction::RefreshTokenThenDial { .. }
            ),
            "a close code is acted on as on any other day"
        );
    }
}
