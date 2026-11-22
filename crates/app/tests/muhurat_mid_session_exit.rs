//! Audit finding 3 (2026-11-14): an exit during a Muhurat evening session
//! withholds the open candle buckets instead of force-sealing them.
//!
//! `is_mid_session_exit` knew only the regular 09:00 to 15:40 IST window, so
//! a deploy or restart at 18:45 IST on a Muhurat date force-sealed every open
//! bucket and wrote bars missing the rest of the session as complete ones.
//! Both lane exits now ask `mid_session_exit::is_mid_session_exit_now`, which
//! adds the Muhurat session of that IST date from the installed trading
//! calendar (`[[trading.muhurat_trading_dates]]`).
//!
//! Its own binary because the calendar `OnceLock` is process-wide. This
//! process never sets the Muhurat process flag (it reads `false`), so every
//! Muhurat answer below comes from the calendar's per-date entry.
//!
//! The 18:00 to 19:00 session on 2026-11-08 is synthetic (the shipped 2026
//! session is 14:05 to 15:40, inside the regular window).

use std::sync::Arc;

use tickvault_app::dhan_feed_stack::mid_session_exit::{
    is_mid_session_exit_at, muhurat_exit_window,
};
use tickvault_common::config::{MuhuratTradingEntry, TradingConfig};
use tickvault_common::trading_calendar::TradingCalendar;

fn entry(date: &str, start: &str, end: &str) -> MuhuratTradingEntry {
    MuhuratTradingEntry {
        date: date.to_string(),
        name: "Diwali".to_string(),
        session_start_ist: start.to_string(),
        session_end_ist: end.to_string(),
    }
}

/// Installs a calendar with an 18:00-19:00 Muhurat session on 2026-11-08 and
/// a Muhurat date with no published times on 2025-10-21, and a
/// daytime 14:05-15:40 session (the shipped 2026 shape) on 2026-11-10, an
/// 18:15-19:45 session on 2026-11-15 and a 14:30-16:00 session on 2026-11-22
/// (audit round 4: both outrun the fold, one starts after it).
/// Idempotent.
fn install_calendar() {
    let cfg = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![],
        muhurat_trading_dates: vec![
            entry("2026-11-08", "18:00", "19:00"),
            entry("2025-10-21", "", ""),
            entry("2026-11-10", "14:05", "15:40"),
            entry("2026-11-15", "18:15", "19:45"),
            entry("2026-11-22", "14:30", "16:00"),
        ],
        nse_mock_trading_dates: vec![],
    };
    let calendar = TradingCalendar::from_config(&cfg).expect("calendar");
    let _first =
        tickvault_common::market_hours::set_market_calendar_for_session(Arc::new(calendar));
}

/// IST epoch seconds of `y-mo-d h:m:s` IST.
fn ist(y: i32, mo: u32, d: u32, h: u32, m: u32, s: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)
        .and_then(|day| day.and_hms_opt(h, m, s))
        .map(|t| t.and_utc().timestamp())
        .expect("valid time")
}

#[test]
fn test_regression_is_mid_session_exit_at_1845_on_a_muhurat_date_withholds() {
    install_calendar();
    assert!(
        is_mid_session_exit_at(ist(2026, 11, 8, 18, 45, 0)),
        "18:45 on the Muhurat date is inside its session: withhold, never force-seal"
    );
    assert!(
        !is_mid_session_exit_at(ist(2026, 11, 9, 18, 45, 0)),
        "18:45 on an ordinary date is after the session: force-seal"
    );
    assert_eq!(
        muhurat_exit_window(ist(2026, 11, 8, 18, 45, 0)),
        Some((18 * 3_600, 19 * 3_600 + 30 * 60))
    );
    assert_eq!(muhurat_exit_window(ist(2026, 11, 9, 18, 45, 0)), None);
}

/// Audit round 2 (2026-11-14): the fold admits Muhurat trades over the fixed
/// `[18:00, 19:30)` window whatever the configured times, so an exit after
/// an 18:00 to 19:00 session withholds until 19:30, as an exit at 15:32
/// withholds on a regular day; from 19:30 it force-seals.
#[test]
fn test_muhurat_exit_window_ends_where_the_fold_stops_admitting() {
    install_calendar();
    assert!(!is_mid_session_exit_at(ist(2026, 11, 8, 17, 59, 59)));
    assert!(is_mid_session_exit_at(ist(2026, 11, 8, 18, 0, 0)));
    assert!(is_mid_session_exit_at(ist(2026, 11, 8, 18, 59, 59)));
    assert!(
        is_mid_session_exit_at(ist(2026, 11, 8, 19, 2, 0)),
        "19:02 after a 19:00 close: the fold still takes the session's trades"
    );
    assert!(is_mid_session_exit_at(ist(2026, 11, 8, 19, 29, 59)));
    assert!(
        !is_mid_session_exit_at(ist(2026, 11, 8, 19, 30, 0)),
        "from the fold's 19:30 end an exit force-seals"
    );
}

/// A Muhurat session inside the regular day is not widened to the evening:
/// the regular window already covers its tail.
#[test]
fn test_a_daytime_muhurat_session_is_not_widened_to_the_evening() {
    install_calendar();
    assert!(is_mid_session_exit_at(ist(2026, 11, 10, 15, 39, 59)));
    assert!(!is_mid_session_exit_at(ist(2026, 11, 10, 15, 40, 0)));
    assert!(!is_mid_session_exit_at(ist(2026, 11, 10, 18, 45, 0)));
    assert!(!is_mid_session_exit_at(ist(2026, 11, 10, 19, 15, 0)));
}

#[test]
fn test_a_muhurat_date_without_published_times_uses_the_writers_evening_window() {
    install_calendar();
    assert!(is_mid_session_exit_at(ist(2025, 10, 21, 18, 45, 0)));
    assert!(is_mid_session_exit_at(ist(2025, 10, 21, 19, 29, 59)));
    assert!(!is_mid_session_exit_at(ist(2025, 10, 21, 19, 30, 0)));
    assert!(!is_mid_session_exit_at(ist(2025, 10, 22, 18, 45, 0)));
}

#[test]
fn test_the_regular_window_is_unchanged_on_every_kind_of_date() {
    install_calendar();
    for (y, mo, d) in [(2026, 11, 8), (2026, 11, 9), (2025, 10, 21), (2026, 11, 14)] {
        assert!(!is_mid_session_exit_at(ist(y, mo, d, 8, 59, 59)));
        assert!(is_mid_session_exit_at(ist(y, mo, d, 9, 0, 0)));
        assert!(is_mid_session_exit_at(ist(y, mo, d, 12, 0, 0)));
        assert!(is_mid_session_exit_at(ist(y, mo, d, 15, 39, 59)));
        assert!(
            !is_mid_session_exit_at(ist(y, mo, d, 15, 40, 0)),
            "15:40 is after the regular session"
        );
        assert!(!is_mid_session_exit_at(ist(y, mo, d, 16, 30, 0)));
    }
}

/// Audit round 4 (2026-11-14): the exit window is where the fold admits,
/// never the configured times. An 18:15 to 19:45 session withholds from the
/// fold's 18:00 open (the pre-open packets fold from there, so an exit at
/// 18:10 must not force-seal buckets missing the session) and force-seals
/// from its 19:30 close (an exit at 19:35 must not withhold buckets nothing
/// can complete).
#[test]
fn test_regression_a_muhurat_session_past_the_fold_is_cut_to_the_fold() {
    install_calendar();
    assert_eq!(
        muhurat_exit_window(ist(2026, 11, 15, 18, 45, 0)),
        Some((18 * 3_600, 19 * 3_600 + 30 * 60))
    );
    assert!(!is_mid_session_exit_at(ist(2026, 11, 15, 17, 59, 59)));
    assert!(
        is_mid_session_exit_at(ist(2026, 11, 15, 18, 10, 0)),
        "18:10 before an 18:15 start: the fold opened buckets at 18:00"
    );
    assert!(is_mid_session_exit_at(ist(2026, 11, 15, 19, 29, 59)));
    assert!(
        !is_mid_session_exit_at(ist(2026, 11, 15, 19, 35, 0)),
        "19:35 before a 19:45 end: the fold closed at 19:30, force-seal"
    );
}

/// A daytime session running past 15:40 adds nothing after the fold's 15:40
/// close: an exit at 15:45 force-seals.
#[test]
fn test_regression_a_daytime_muhurat_session_past_1540_force_seals_after_1540() {
    install_calendar();
    assert_eq!(muhurat_exit_window(ist(2026, 11, 22, 15, 45, 0)), None);
    assert!(is_mid_session_exit_at(ist(2026, 11, 22, 15, 39, 59)));
    assert!(!is_mid_session_exit_at(ist(2026, 11, 22, 15, 45, 0)));
    assert!(!is_mid_session_exit_at(ist(2026, 11, 22, 15, 59, 59)));
}
