//! Recovery of the paused D11 wiring fix: a listed Sunday Muhurat session
//! needs today's artifact and late widening. Calendar eligibility stays canonical.
#![cfg(test)]

use chrono::{NaiveDate, TimeZone};
use tickvault_common::config::{MuhuratTradingEntry, NseHolidayEntry, TradingConfig};
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};

#[test]
fn test_regression_universe_wait_and_widen_share_one_calendar_session_day() {
    let main = include_str!("../src/main.rs");
    let code: String = main
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(str::split_whitespace)
        .collect();
    assert!(code.contains(
        "letuniverse_day_ist=chrono::Utc::now().with_timezone(&tickvault_common::trading_calendar::ist_offset()).date_naive();"
    ));
    assert!(code.contains("letuniverse_date_ist=universe_day_ist.to_string();"));
    assert!(
        code.contains("letuniverse_session_day=trading_calendar.is_session_day(universe_day_ist);")
    );
    assert!(code.contains(
        "await_mapping_artifact(&config.dhan_universe,&universe_date_ist,universe_collapse_expected,universe_session_day,)"
    ));
    assert!(code.contains(
        "widen_universe:(tickvault_app::dhan_live_universe::live_universe_widen_pending()&&universe_session_day)"
    ));
    // The existing collapse-alert and re-judgement policy remains separate.
    assert!(
        code.contains(
            "collapse_is_expected_for_this_boot(trading_calendar.is_trading_day_today(),"
        )
    );
    assert!(code.contains("letuniverse_collapse_expected_after_wait=universe_collapse_expected&&"));
}

#[test]
fn test_regression_calendar_session_day_covers_muhurat_and_ist_date_boundary() {
    let config = TradingConfig {
        market_open_time: "09:00:00".into(),
        market_close_time: "15:30:00".into(),
        order_cutoff_time: "15:29:00".into(),
        data_collection_start: "09:00:00".into(),
        data_collection_end: "15:30:00".into(),
        timezone: "Asia/Kolkata".into(),
        max_orders_per_second: 10,
        nse_holidays: vec![
            NseHolidayEntry {
                date: "2026-11-09".into(),
                name: "Closed holiday".into(),
            },
            NseHolidayEntry {
                date: "2026-11-10".into(),
                name: "Special session holiday".into(),
            },
        ],
        muhurat_trading_dates: vec![
            MuhuratTradingEntry {
                date: "2026-11-08".into(),
                name: "Sunday Muhurat".into(),
                ..Default::default()
            },
            MuhuratTradingEntry {
                date: "2026-11-10".into(),
                name: "Weekday Muhurat".into(),
                ..Default::default()
            },
        ],
        nse_mock_trading_dates: vec![NseHolidayEntry {
            date: "2026-11-07".into(),
            name: "Mock only".into(),
        }],
    };
    let calendar = TradingCalendar::from_config(&config).unwrap();
    for (day, expected) in [
        (7, false),
        (8, true),
        (9, false),
        (10, true),
        (11, true),
        (15, false),
    ] {
        let date = NaiveDate::from_ymd_opt(2026, 11, day).unwrap();
        assert_eq!(calendar.is_session_day(date), expected, "{date}");
    }
    let sunday = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
    assert!(!calendar.is_trading_day(sunday));
    assert!(calendar.is_muhurat_trading_day(sunday));
    // An artifact date must switch with the same IST date used by eligibility.
    for (day, hour, minute, second, date_text, expected) in [
        (7, 18, 29, 59, "2026-11-07", false),
        (7, 18, 30, 0, "2026-11-08", true),
        (8, 18, 29, 59, "2026-11-08", true),
        (8, 18, 30, 0, "2026-11-09", false),
    ] {
        let instant = chrono::Utc
            .with_ymd_and_hms(2026, 11, day, hour, minute, second)
            .unwrap();
        let date = instant.with_timezone(&ist_offset()).date_naive();
        assert_eq!(date.to_string(), date_text);
        assert_eq!(calendar.is_session_day(date), expected);
    }
}
