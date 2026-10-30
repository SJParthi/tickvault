//! Muhurat session as a calendar fact (owner 2026-10-30: "go ahead and fix
//! the muhurat risk dude").
//!
//! `[[trading.muhurat_trading_dates]]` in `config/base.toml` is the ONE source
//! of the Muhurat session: its date makes the day a session day, and its
//! `session_start_ist` / `session_end_ist` drive the box schedule, the deploy
//! freeze and the liveness alarm arming (terraform and the workflows are
//! pinned to it). This binary checks the calendar treats a listed session
//! like a trading day, never an ordinary weekend or holiday, and that every
//! configured session sits inside the windows the app's writers accept.

use chrono::NaiveDate;
use tickvault_common::config::{MuhuratTradingEntry, NseHolidayEntry, TradingConfig};
use tickvault_common::muhurat::session_is_inside_capture_windows;
use tickvault_common::trading_calendar::{TradingCalendar, parse_muhurat_session_times};

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
}

fn muhurat(date: &str, start: &str, end: &str) -> MuhuratTradingEntry {
    MuhuratTradingEntry {
        date: date.to_string(),
        name: "Muhurat".to_string(),
        session_start_ist: start.to_string(),
        session_end_ist: end.to_string(),
    }
}

fn config(muhurat_dates: Vec<MuhuratTradingEntry>) -> TradingConfig {
    TradingConfig {
        market_open_time: "09:15:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "16:00:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![
            NseHolidayEntry {
                date: "2026-11-10".to_string(),
                name: "Diwali Balipratipada".to_string(),
            },
            NseHolidayEntry {
                date: "2026-11-24".to_string(),
                name: "Guru Nanak Jayanti".to_string(),
            },
        ],
        muhurat_trading_dates: muhurat_dates,
        nse_mock_trading_dates: vec![],
    }
}

/// The `[[trading.muhurat_trading_dates]]` entries of the tracked config.
fn base_toml_muhurat_entries() -> Vec<MuhuratTradingEntry> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    let raw = std::fs::read_to_string(&path).expect("read config/base.toml");
    let value: toml::Value = toml::from_str(&raw).expect("config/base.toml parses");
    let entries = value
        .get("trading")
        .and_then(|t| t.get("muhurat_trading_dates"))
        .cloned()
        .expect("config/base.toml has [[trading.muhurat_trading_dates]]");
    entries.try_into().expect("Muhurat entries deserialize")
}

#[test]
fn test_sunday_muhurat_is_a_session_day_with_its_times() {
    let cal = TradingCalendar::from_config(&config(vec![muhurat("2026-11-08", "14:05", "15:40")]))
        .expect("valid");
    let sunday = day(2026, 11, 8);
    assert!(
        !cal.is_trading_day(sunday),
        "a Sunday is never a regular day"
    );
    assert!(
        cal.is_session_day(sunday),
        "a listed Muhurat Sunday is a session day"
    );
    assert_eq!(
        cal.muhurat_session_secs(sunday),
        Some((14 * 3600 + 5 * 60, 15 * 3600 + 40 * 60))
    );
}

#[test]
fn test_ordinary_sunday_and_weekday_holiday_stay_closed() {
    let cal = TradingCalendar::from_config(&config(vec![muhurat("2026-11-08", "14:05", "15:40")]))
        .expect("valid");
    let ordinary_sunday = day(2026, 11, 15);
    assert!(!cal.is_session_day(ordinary_sunday));
    assert_eq!(cal.muhurat_session_secs(ordinary_sunday), None);
    let weekday_holiday = day(2026, 11, 10);
    assert!(!cal.is_trading_day(weekday_holiday));
    assert!(!cal.is_session_day(weekday_holiday));
    let weekday = day(2026, 11, 9);
    assert!(
        cal.is_session_day(weekday),
        "a regular weekday stays a session day"
    );
    assert_eq!(cal.muhurat_session_secs(weekday), None);
}

#[test]
fn test_muhurat_on_a_weekday_holiday_is_a_session_day() {
    let cal = TradingCalendar::from_config(&config(vec![muhurat("2026-11-10", "18:00", "19:15")]))
        .expect("valid");
    let date = day(2026, 11, 10);
    assert!(cal.is_holiday(date));
    assert!(cal.is_session_day(date));
    let (start, end) = cal.muhurat_session_secs(date).expect("times set");
    assert!(session_is_inside_capture_windows(start, end));
}

#[test]
fn test_unpublished_times_keep_the_date_but_no_session_times() {
    let cal = TradingCalendar::from_config(&config(vec![muhurat("2026-11-08", "", "")]))
        .expect("empty times are allowed until NSE publishes them");
    assert!(cal.is_session_day(day(2026, 11, 8)));
    assert_eq!(cal.muhurat_session_secs(day(2026, 11, 8)), None);
}

#[test]
fn test_malformed_session_times_refuse_the_calendar() {
    for (start, end) in [
        ("14:05", ""),
        ("", "15:40"),
        ("9:05", "15:40"),
        ("14:05:00", "15:40"),
        ("24:00", "25:00"),
        ("15:40", "14:05"),
        ("14:05", "14:05"),
    ] {
        assert!(
            TradingCalendar::from_config(&config(vec![muhurat("2026-11-08", start, end)])).is_err(),
            "start '{start}' end '{end}' must be refused"
        );
    }
}

#[test]
fn test_parse_muhurat_session_times_evening_window() {
    assert_eq!(
        parse_muhurat_session_times("2024-11-01", "18:00", "19:15").expect("valid"),
        Some((18 * 3600, 19 * 3600 + 15 * 60))
    );
    assert_eq!(
        parse_muhurat_session_times("2026-11-08", " ", "").expect("blank is unset"),
        None
    );
}

#[test]
fn every_configured_muhurat_session_is_inside_the_capture_windows() {
    let entries = base_toml_muhurat_entries();
    assert!(
        !entries.is_empty(),
        "config/base.toml lists no Muhurat date"
    );
    for entry in &entries {
        let parsed = parse_muhurat_session_times(
            &entry.date,
            &entry.session_start_ist,
            &entry.session_end_ist,
        )
        .unwrap_or_else(|e| panic!("config/base.toml Muhurat {}: {e}", entry.date));
        if let Some((start, end)) = parsed {
            assert!(
                session_is_inside_capture_windows(start, end),
                "config/base.toml Muhurat {} session {}-{} is not inside one window the tick, \
                 depth and candle writers accept (09:00-15:40 or 18:00-19:30 IST): part of the \
                 session would be refused. Widen the writers' window first.",
                entry.date,
                entry.session_start_ist,
                entry.session_end_ist
            );
        }
    }
}

#[test]
fn the_2026_muhurat_session_times_are_published_in_config() {
    let entries = base_toml_muhurat_entries();
    let diwali = entries
        .iter()
        .find(|e| e.date == "2026-11-08")
        .expect("2026-11-08 Muhurat listed in config/base.toml");
    assert!(
        !diwali.session_start_ist.is_empty() && !diwali.session_end_ist.is_empty(),
        "2026-11-08 must carry its session times: without them the box is not started"
    );
}
