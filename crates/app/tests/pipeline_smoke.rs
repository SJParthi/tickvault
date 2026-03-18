//! Smoke integration tests for the app crate.
//!
//! The app crate is a binary-only crate (no lib.rs), so these tests
//! verify that key types from dependencies compile and behave as expected.

use dhan_live_trader_common::config::NseHolidayEntry;
use dhan_live_trader_common::constants::{
    DEDUP_RING_BUFFER_POWER, IST_UTC_OFFSET_SECONDS, MINIMUM_VALID_EXCHANGE_TIMESTAMP,
};
use dhan_live_trader_common::trading_calendar::TradingCalendar;

// ---------------------------------------------------------------------------
// Constants smoke tests
// ---------------------------------------------------------------------------

#[test]
fn test_ist_utc_offset_is_19800_seconds() {
    // IST = UTC+5:30 = 5*3600 + 30*60 = 19800
    assert_eq!(IST_UTC_OFFSET_SECONDS, 19800);
}

#[test]
fn test_dedup_ring_buffer_power_in_valid_range() {
    // DEDUP_RING_BUFFER_POWER must be in [8, 24] per TickDedupRing::new() contract.
    assert!(
        (8..=24).contains(&DEDUP_RING_BUFFER_POWER),
        "DEDUP_RING_BUFFER_POWER={DEDUP_RING_BUFFER_POWER} out of valid range [8, 24]"
    );
}

#[test]
fn test_minimum_valid_exchange_timestamp_positive() {
    // Must be > 0 (some epoch after Unix epoch start)
    const { assert!(MINIMUM_VALID_EXCHANGE_TIMESTAMP > 0) };
    // Must be before year 2040 (~2208988800)
    const { assert!(MINIMUM_VALID_EXCHANGE_TIMESTAMP < 2_208_988_800) };
}

// ---------------------------------------------------------------------------
// TradingCalendar smoke tests (with standard 2026 holidays)
// ---------------------------------------------------------------------------

fn standard_2026_holidays() -> Vec<NseHolidayEntry> {
    vec![
        NseHolidayEntry {
            date: "2026-01-26".to_string(),
            name: "Republic Day".to_string(),
        },
        NseHolidayEntry {
            date: "2026-03-03".to_string(),
            name: "Holi".to_string(),
        },
        NseHolidayEntry {
            date: "2026-03-26".to_string(),
            name: "Shri Ram Navami".to_string(),
        },
        NseHolidayEntry {
            date: "2026-03-31".to_string(),
            name: "Shri Mahavir Jayanti".to_string(),
        },
        NseHolidayEntry {
            date: "2026-04-03".to_string(),
            name: "Good Friday".to_string(),
        },
        NseHolidayEntry {
            date: "2026-04-14".to_string(),
            name: "Ambedkar Jayanti".to_string(),
        },
        NseHolidayEntry {
            date: "2026-05-01".to_string(),
            name: "Maharashtra Day".to_string(),
        },
        NseHolidayEntry {
            date: "2026-05-28".to_string(),
            name: "Bakri Eid".to_string(),
        },
        NseHolidayEntry {
            date: "2026-06-26".to_string(),
            name: "Muharram".to_string(),
        },
        NseHolidayEntry {
            date: "2026-09-14".to_string(),
            name: "Ganesh Chaturthi".to_string(),
        },
        NseHolidayEntry {
            date: "2026-10-02".to_string(),
            name: "Gandhi Jayanti".to_string(),
        },
        NseHolidayEntry {
            date: "2026-10-20".to_string(),
            name: "Dussehra".to_string(),
        },
        NseHolidayEntry {
            date: "2026-11-10".to_string(),
            name: "Diwali Balipratipada".to_string(),
        },
        NseHolidayEntry {
            date: "2026-11-24".to_string(),
            name: "Guru Nanak Gurpurb".to_string(),
        },
        NseHolidayEntry {
            date: "2026-12-25".to_string(),
            name: "Christmas".to_string(),
        },
    ]
}

fn test_trading_config() -> dhan_live_trader_common::config::TradingConfig {
    dhan_live_trader_common::config::TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: standard_2026_holidays(),
        muhurat_trading_dates: vec![NseHolidayEntry {
            date: "2026-11-08".to_string(),
            name: "Diwali 2026".to_string(),
        }],
    }
}

#[test]
fn test_trading_calendar_construction_succeeds() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config);
    assert!(
        calendar.is_ok(),
        "TradingCalendar construction failed: {:?}",
        calendar.err()
    );
}

#[test]
fn test_trading_calendar_weekday_is_trading_day() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config).expect("valid config"); // APPROVED: test-only
    // Monday 2026-01-05 is a weekday with no holiday
    let monday = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).expect("valid date"); // APPROVED: test-only
    assert!(calendar.is_trading_day(monday));
}

#[test]
fn test_trading_calendar_weekend_is_not_trading_day() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config).expect("valid config"); // APPROVED: test-only
    // Saturday 2026-01-03
    let saturday = chrono::NaiveDate::from_ymd_opt(2026, 1, 3).expect("valid date"); // APPROVED: test-only
    assert!(!calendar.is_trading_day(saturday));
}

#[test]
fn test_trading_calendar_holiday_is_not_trading_day() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config).expect("valid config"); // APPROVED: test-only
    // Republic Day 2026-01-26 (Monday)
    let republic_day = chrono::NaiveDate::from_ymd_opt(2026, 1, 26).expect("valid date"); // APPROVED: test-only
    assert!(!calendar.is_trading_day(republic_day));
}
