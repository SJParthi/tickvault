//! Property-based tests for shared types in the common crate.
//!
//! Uses proptest to verify invariants:
//! - ExchangeSegment::from_byte() round-trips for all valid byte codes
//! - ExchangeSegment::from_byte() returns None for all invalid byte codes
//! - OrderStatus serde round-trip consistency
//! - TradingCalendar never panics on arbitrary dates
//! - Config validation never panics on arbitrary strings

use chrono::Datelike;
use proptest::prelude::*;
use tickvault_common::config::{NseHolidayEntry, TradingConfig};
use tickvault_common::order_types::OrderStatus;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::ExchangeSegment;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// All valid ExchangeSegment byte codes from Dhan protocol.
/// Note: code 6 is intentionally absent (gap in Dhan mapping).
const VALID_SEGMENT_CODES: [u8; 8] = [0, 1, 2, 3, 4, 5, 7, 8];

/// Creates a minimal TradingConfig for calendar tests.
fn make_minimal_trading_config() -> TradingConfig {
    TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![
            NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            },
            NseHolidayEntry {
                date: "2026-03-03".to_string(),
                name: "Holi".to_string(),
            },
        ],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    }
}

// ---------------------------------------------------------------------------
// Property: ExchangeSegment::from_byte() round-trips for all valid codes
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_exchange_segment_roundtrip(
        code in prop_oneof![
            Just(0_u8), Just(1), Just(2), Just(3), Just(4), Just(5), Just(7), Just(8)
        ]
    ) {
        let segment = ExchangeSegment::from_byte(code)
            .unwrap_or_else(|| panic!("from_byte({code}) must succeed for valid code"));
        let back = segment.binary_code();
        prop_assert_eq!(back, code, "binary_code() must roundtrip through from_byte()");
    }
}

// ---------------------------------------------------------------------------
// Property: ExchangeSegment::from_byte() returns None for invalid codes
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_exchange_segment_invalid_returns_none(code in 0_u8..=255) {
        let result = ExchangeSegment::from_byte(code);
        if VALID_SEGMENT_CODES.contains(&code) {
            prop_assert!(result.is_some(), "valid code {code} should return Some");
        } else {
            prop_assert!(result.is_none(), "invalid code {code} should return None (got {result:?})");
        }
    }
}

// ---------------------------------------------------------------------------
// Property: ExchangeSegment Display/as_str never panics
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_exchange_segment_display_never_panics(code in 0_u8..=255) {
        if let Some(segment) = ExchangeSegment::from_byte(code) {
            let display = format!("{segment}");
            let as_str = segment.as_str();
            prop_assert_eq!(&display, as_str);
            prop_assert!(!as_str.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// Property: OrderStatus serde roundtrip consistency
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_order_status_serde_roundtrip(
        variant in prop_oneof![
            Just(OrderStatus::Transit),
            Just(OrderStatus::Pending),
            Just(OrderStatus::Confirmed),
            Just(OrderStatus::PartTraded),
            Just(OrderStatus::Traded),
            Just(OrderStatus::Cancelled),
            Just(OrderStatus::Rejected),
            Just(OrderStatus::Expired),
            Just(OrderStatus::Closed),
            Just(OrderStatus::Triggered),
        ]
    ) {
        let json = serde_json::to_string(&variant).unwrap();
        let deserialized: OrderStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(variant, deserialized);
    }
}

// ---------------------------------------------------------------------------
// Property: OrderStatus as_str matches serde serialization
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_order_status_as_str_matches_serde(
        variant in prop_oneof![
            Just(OrderStatus::Transit),
            Just(OrderStatus::Pending),
            Just(OrderStatus::Confirmed),
            Just(OrderStatus::PartTraded),
            Just(OrderStatus::Traded),
            Just(OrderStatus::Cancelled),
            Just(OrderStatus::Rejected),
            Just(OrderStatus::Expired),
            Just(OrderStatus::Closed),
            Just(OrderStatus::Triggered),
        ]
    ) {
        let as_str = variant.as_str();
        // serde serialization wraps in quotes: "TRANSIT"
        let expected_json = format!("\"{as_str}\"");
        let actual_json = serde_json::to_string(&variant).unwrap();
        prop_assert_eq!(actual_json, expected_json);
    }
}

// ---------------------------------------------------------------------------
// Property: TradingCalendar::is_trading_day never panics on any date
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_trading_calendar_never_panics(
        year in 2020_i32..2030,
        month in 1_u32..=12,
        day in 1_u32..=31,
    ) {
        let config = make_minimal_trading_config();
        let calendar = TradingCalendar::from_config(&config).unwrap();

        if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) {
            // Must not panic regardless of date
            let _ = calendar.is_trading_day(date);
            let _ = calendar.is_holiday(date);
            let _ = calendar.is_muhurat_trading_day(date);
            let _ = calendar.next_trading_day(date);
        }
        // Invalid dates (e.g., Feb 31) just return None from from_ymd_opt — not our concern
    }
}

// ---------------------------------------------------------------------------
// Property: TradingCalendar weekends are never trading days
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_weekends_never_trading_days(
        year in 2020_i32..2030,
        month in 1_u32..=12,
        day in 1_u32..=31,
    ) {
        let config = make_minimal_trading_config();
        let calendar = TradingCalendar::from_config(&config).unwrap();

        if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) {
            let weekday = date.weekday();
            if weekday == chrono::Weekday::Sat || weekday == chrono::Weekday::Sun {
                prop_assert!(!calendar.is_trading_day(date),
                    "weekends must never be trading days: {date}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: Config feed_mode parsing never panics
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_subscription_config_feed_mode_never_panics(
        feed_mode in "[a-zA-Z0-9 ]{0,20}"
    ) {
        let config = tickvault_common::config::SubscriptionConfig {
            feed_mode,
            ..tickvault_common::config::SubscriptionConfig::default()
        };
        // parsed_feed_mode must not panic — it returns Result
        let _ = config.parsed_feed_mode();
    }
}

// ---------------------------------------------------------------------------
// Property: TradingCalendar::from_config never panics on invalid date strings
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn proptest_calendar_from_config_never_panics_on_invalid_dates(
        date_str in "[0-9a-z\\-]{0,15}"
    ) {
        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: date_str,
                name: "Test".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };

        // Must never panic — returns Result
        let _ = TradingCalendar::from_config(&config);
    }
}
