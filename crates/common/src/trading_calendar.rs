//! NSE Trading Calendar — holiday awareness and trading day classification.
//!
//! Built at startup from config. Provides O(1) `is_trading_day()` checks
//! via `HashSet<NaiveDate>`. No allocation on the hot path — the set is
//! constructed once during boot and queried read-only thereafter.

use std::collections::HashSet;

use anyhow::{Result, bail};
use chrono::{Datelike, FixedOffset, NaiveDate, Utc, Weekday};

use crate::config::TradingConfig;
use crate::constants::IST_UTC_OFFSET_SECONDS;

/// Pre-built trading calendar with O(1) holiday lookups.
///
/// Constructed once at startup from `TradingConfig` holiday lists.
/// Immutable after creation — safe to share across threads via `Arc`.
#[derive(Debug, Clone)]
pub struct TradingCalendar {
    /// All NSE holidays (combined 2025 + 2026).
    holidays: HashSet<NaiveDate>,
    /// Muhurat Trading dates (special evening sessions on otherwise closed days).
    muhurat_dates: HashSet<NaiveDate>,
}

impl TradingCalendar {
    /// Builds a `TradingCalendar` from validated config strings.
    ///
    /// # Errors
    /// Returns error if any date string fails to parse as `YYYY-MM-DD`.
    pub fn from_config(trading: &TradingConfig) -> Result<Self> {
        let mut holidays = HashSet::new();

        for date_str in trading
            .nse_holidays_2025
            .iter()
            .chain(trading.nse_holidays_2026.iter())
        {
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .map_err(|e| anyhow::anyhow!("invalid NSE holiday date '{}': {}", date_str, e))?;

            // Sanity: holidays should be weekdays (Sat/Sun are already non-trading).
            if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
                bail!(
                    "NSE holiday '{}' ({}) falls on a weekend — only list weekday holidays",
                    date_str,
                    date.weekday()
                );
            }

            holidays.insert(date);
        }

        let mut muhurat_dates = HashSet::new();
        for date_str in &trading.muhurat_trading_dates {
            let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").map_err(|e| {
                anyhow::anyhow!("invalid Muhurat trading date '{}': {}", date_str, e)
            })?;
            muhurat_dates.insert(date);
        }

        Ok(Self {
            holidays,
            muhurat_dates,
        })
    }

    /// Returns `true` if the given date is a regular NSE trading day.
    ///
    /// A date is a trading day if:
    /// 1. It is NOT Saturday or Sunday
    /// 2. It is NOT in the NSE holiday list
    ///
    /// Note: Muhurat Trading sessions on holidays are special — they have
    /// limited hours and are handled separately. This function returns `false`
    /// for Muhurat dates that are also holidays (e.g., Diwali).
    pub fn is_trading_day(&self, date: NaiveDate) -> bool {
        if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            return false;
        }
        !self.holidays.contains(&date)
    }

    /// Returns `true` if today (IST) is a regular NSE trading day.
    pub fn is_trading_day_today(&self) -> bool {
        self.is_trading_day(today_ist())
    }

    /// Returns `true` if the given date has a Muhurat Trading session.
    pub fn is_muhurat_trading_day(&self, date: NaiveDate) -> bool {
        self.muhurat_dates.contains(&date)
    }

    /// Returns `true` if today (IST) has a Muhurat Trading session.
    pub fn is_muhurat_trading_today(&self) -> bool {
        self.is_muhurat_trading_day(today_ist())
    }

    /// Returns `true` if the given date is an NSE holiday.
    pub fn is_holiday(&self, date: NaiveDate) -> bool {
        self.holidays.contains(&date)
    }

    /// Returns the next regular trading day on or after the given date.
    ///
    /// Useful for scheduling: "when is the next day we should start up?"
    pub fn next_trading_day(&self, from: NaiveDate) -> NaiveDate {
        let mut candidate = from;
        // Safety: loop will terminate within a few days (max consecutive
        // non-trading days is ~4 for long weekends + holidays).
        loop {
            if self.is_trading_day(candidate) {
                return candidate;
            }
            candidate = candidate.succ_opt().unwrap_or(candidate);
        }
    }

    /// Total number of holidays loaded.
    pub fn holiday_count(&self) -> usize {
        self.holidays.len()
    }

    /// Total number of Muhurat trading dates loaded.
    pub fn muhurat_count(&self) -> usize {
        self.muhurat_dates.len()
    }
}

/// Returns today's date in IST.
fn today_ist() -> NaiveDate {
    let ist =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid");
    Utc::now().with_timezone(&ist).date_naive()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_config() -> TradingConfig {
        TradingConfig {
            market_open_time: "09:15:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays_2025: vec![
                "2025-02-26".to_string(), // Wednesday — Mahashivratri
                "2025-03-14".to_string(), // Friday — Holi
                "2025-10-21".to_string(), // Tuesday — Diwali
            ],
            nse_holidays_2026: vec![
                "2026-01-26".to_string(), // Monday — Republic Day
                "2026-03-03".to_string(), // Tuesday — Holi
            ],
            muhurat_trading_dates: vec![
                "2025-10-21".to_string(), // Diwali 2025
            ],
        }
    }

    #[test]
    fn test_from_config_builds_calendar() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        assert_eq!(cal.holiday_count(), 5);
        assert_eq!(cal.muhurat_count(), 1);
    }

    #[test]
    fn test_weekday_non_holiday_is_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2025-02-25 is Tuesday, not a holiday
        let date = NaiveDate::from_ymd_opt(2025, 2, 25).unwrap();
        assert!(cal.is_trading_day(date));
    }

    #[test]
    fn test_holiday_is_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2025-02-26 is Mahashivratri
        let date = NaiveDate::from_ymd_opt(2025, 2, 26).unwrap();
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_saturday_is_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let date = NaiveDate::from_ymd_opt(2025, 3, 1).unwrap(); // Saturday
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_sunday_is_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let date = NaiveDate::from_ymd_opt(2025, 3, 2).unwrap(); // Sunday
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_muhurat_day_detected() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let date = NaiveDate::from_ymd_opt(2025, 10, 21).unwrap();
        assert!(cal.is_muhurat_trading_day(date));
        // Diwali is a holiday — not a regular trading day
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_is_holiday() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        assert!(cal.is_holiday(NaiveDate::from_ymd_opt(2026, 1, 26).unwrap()));
        assert!(!cal.is_holiday(NaiveDate::from_ymd_opt(2026, 1, 27).unwrap()));
    }

    #[test]
    fn test_next_trading_day_skips_weekend() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // Friday 2025-02-28 is a trading day
        let friday = NaiveDate::from_ymd_opt(2025, 2, 28).unwrap();
        assert_eq!(cal.next_trading_day(friday), friday);
        // Saturday 2025-03-01 → next trading day is Monday 2025-03-03
        let saturday = NaiveDate::from_ymd_opt(2025, 3, 1).unwrap();
        assert_eq!(
            cal.next_trading_day(saturday),
            NaiveDate::from_ymd_opt(2025, 3, 3).unwrap()
        );
    }

    #[test]
    fn test_next_trading_day_skips_holiday() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2025-02-26 (Wed) is Mahashivratri, next trading day is 2025-02-27 (Thu)
        let holiday = NaiveDate::from_ymd_opt(2025, 2, 26).unwrap();
        assert_eq!(
            cal.next_trading_day(holiday),
            NaiveDate::from_ymd_opt(2025, 2, 27).unwrap()
        );
    }

    #[test]
    fn test_weekend_holiday_in_config_rejected() {
        let mut config = make_test_config();
        config.nse_holidays_2025.push("2025-03-01".to_string()); // Saturday
        let err = TradingCalendar::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("weekend"));
    }

    #[test]
    fn test_invalid_date_string_rejected() {
        let mut config = make_test_config();
        config.nse_holidays_2025.push("not-a-date".to_string());
        let err = TradingCalendar::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("invalid NSE holiday date"));
    }

    #[test]
    fn test_empty_holidays_all_weekdays_are_trading_days() {
        let mut config = make_test_config();
        config.nse_holidays_2025.clear();
        config.nse_holidays_2026.clear();
        config.muhurat_trading_dates.clear();
        let cal = TradingCalendar::from_config(&config).unwrap();
        assert_eq!(cal.holiday_count(), 0);
        // Any weekday should be a trading day
        let monday = NaiveDate::from_ymd_opt(2025, 3, 3).unwrap();
        assert!(cal.is_trading_day(monday));
    }

    #[test]
    fn test_2026_holidays_loaded() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // Republic Day 2026
        assert!(cal.is_holiday(NaiveDate::from_ymd_opt(2026, 1, 26).unwrap()));
        // Holi 2026
        assert!(cal.is_holiday(NaiveDate::from_ymd_opt(2026, 3, 3).unwrap()));
    }
}
