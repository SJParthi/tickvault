//! NSE Trading Calendar — holiday awareness and trading day classification.
//!
//! Built at startup from config. Provides O(1) `is_trading_day()` checks
//! via `HashSet<NaiveDate>`. No allocation on the hot path — the set is
//! constructed once during boot and queried read-only thereafter.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use chrono::{Datelike, FixedOffset, NaiveDate, Utc, Weekday};

use crate::config::TradingConfig;
use crate::constants::IST_UTC_OFFSET_SECONDS;

/// A holiday entry with parsed date and display name.
#[derive(Debug, Clone)]
pub struct HolidayInfo {
    /// The holiday date.
    pub date: NaiveDate,
    /// Human-readable name (e.g., "Republic Day").
    pub name: String,
    /// Whether this is a Muhurat Trading session (not a regular holiday).
    pub is_muhurat: bool,
}

/// Pre-built trading calendar with O(1) holiday lookups.
///
/// Constructed once at startup from `TradingConfig` holiday lists.
/// Immutable after creation — safe to share across threads via `Arc`.
#[derive(Debug, Clone)]
pub struct TradingCalendar {
    /// All NSE holidays loaded from config.
    holidays: HashSet<NaiveDate>,
    /// Holiday name lookup for display/persistence.
    holiday_names: HashMap<NaiveDate, String>,
    /// Muhurat Trading dates (special evening sessions on otherwise closed days).
    muhurat_dates: HashSet<NaiveDate>,
    /// Muhurat name lookup for display/persistence.
    muhurat_names: HashMap<NaiveDate, String>,
}

impl TradingCalendar {
    /// Builds a `TradingCalendar` from validated config entries.
    ///
    /// # Errors
    /// Returns error if any date string fails to parse as `YYYY-MM-DD`.
    pub fn from_config(trading: &TradingConfig) -> Result<Self> {
        let mut holidays = HashSet::new();
        let mut holiday_names = HashMap::new();

        for entry in &trading.nse_holidays {
            let date = NaiveDate::parse_from_str(&entry.date, "%Y-%m-%d")
                .map_err(|e| anyhow::anyhow!("invalid NSE holiday date '{}': {}", entry.date, e))?;

            // Sanity: holidays should be weekdays (Sat/Sun are already non-trading).
            if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
                bail!(
                    "NSE holiday '{}' ({}) falls on a weekend — only list weekday holidays",
                    entry.date,
                    date.weekday()
                );
            }

            holidays.insert(date);
            holiday_names.insert(date, entry.name.clone());
        }

        let mut muhurat_dates = HashSet::new();
        let mut muhurat_names = HashMap::new();
        for entry in &trading.muhurat_trading_dates {
            let date = NaiveDate::parse_from_str(&entry.date, "%Y-%m-%d").map_err(|e| {
                anyhow::anyhow!("invalid Muhurat trading date '{}': {}", entry.date, e)
            })?;
            muhurat_dates.insert(date);
            muhurat_names.insert(date, entry.name.clone());
        }

        Ok(Self {
            holidays,
            holiday_names,
            muhurat_dates,
            muhurat_names,
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

    /// Returns all holiday and Muhurat entries for persistence/display.
    /// Sorted by date ascending.
    pub fn all_entries(&self) -> Vec<HolidayInfo> {
        let mut entries: Vec<HolidayInfo> =
            Vec::with_capacity(self.holidays.len().saturating_add(self.muhurat_dates.len()));

        for (&date, name) in &self.holiday_names {
            entries.push(HolidayInfo {
                date,
                name: name.clone(),
                is_muhurat: false,
            });
        }

        for (&date, name) in &self.muhurat_names {
            entries.push(HolidayInfo {
                date,
                name: name.clone(),
                is_muhurat: true,
            });
        }

        entries.sort_by_key(|e| e.date);
        entries
    }
}

/// Returns the IST timezone offset (+05:30 / UTC+19800s).
///
/// Use this instead of `FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect(...)` everywhere.
/// The offset value 19800 is within `FixedOffset`'s valid range (compile-time provable constant).
#[allow(clippy::expect_used)] // APPROVED: compile-time provable — IST_UTC_OFFSET_SECONDS (19800) is always valid
pub fn ist_offset() -> FixedOffset {
    FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid") // APPROVED: compile-time provable constant
}

/// Returns today's date in IST.
fn today_ist() -> NaiveDate {
    Utc::now().with_timezone(&ist_offset()).date_naive()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NseHolidayEntry;

    fn make_test_config() -> TradingConfig {
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
                NseHolidayEntry {
                    date: "2026-10-20".to_string(),
                    name: "Dussehra".to_string(),
                },
            ],
            muhurat_trading_dates: vec![NseHolidayEntry {
                date: "2026-11-08".to_string(),
                name: "Diwali 2026".to_string(),
            }],
        }
    }

    #[test]
    fn test_from_config_builds_calendar() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        assert_eq!(cal.holiday_count(), 3);
        assert_eq!(cal.muhurat_count(), 1);
    }

    #[test]
    fn test_weekday_non_holiday_is_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-27 is Tuesday, not a holiday
        let date = NaiveDate::from_ymd_opt(2026, 1, 27).unwrap();
        assert!(cal.is_trading_day(date));
    }

    #[test]
    fn test_holiday_is_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-26 is Republic Day
        let date = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_saturday_is_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap(); // Saturday
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_sunday_is_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(); // Sunday
        assert!(!cal.is_trading_day(date));
    }

    #[test]
    fn test_muhurat_day_detected() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        assert!(cal.is_muhurat_trading_day(date));
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
        // Friday 2026-03-06 is a trading day
        let friday = NaiveDate::from_ymd_opt(2026, 3, 6).unwrap();
        assert_eq!(cal.next_trading_day(friday), friday);
        // Saturday 2026-03-07 → next trading day is Monday 2026-03-09
        let saturday = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        assert_eq!(
            cal.next_trading_day(saturday),
            NaiveDate::from_ymd_opt(2026, 3, 9).unwrap()
        );
    }

    #[test]
    fn test_next_trading_day_skips_holiday() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-26 (Mon) is Republic Day, next trading day is 2026-01-27 (Tue)
        let holiday = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
        assert_eq!(
            cal.next_trading_day(holiday),
            NaiveDate::from_ymd_opt(2026, 1, 27).unwrap()
        );
    }

    #[test]
    fn test_weekend_holiday_in_config_rejected() {
        let mut config = make_test_config();
        config.nse_holidays.push(NseHolidayEntry {
            date: "2026-03-07".to_string(), // Saturday
            name: "Test Weekend".to_string(),
        });
        let err = TradingCalendar::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("weekend"));
    }

    #[test]
    fn test_invalid_date_string_rejected() {
        let mut config = make_test_config();
        config.nse_holidays.push(NseHolidayEntry {
            date: "not-a-date".to_string(),
            name: "Bad Date".to_string(),
        });
        let err = TradingCalendar::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("invalid NSE holiday date"));
    }

    #[test]
    fn test_empty_holidays_all_weekdays_are_trading_days() {
        let mut config = make_test_config();
        config.nse_holidays.clear();
        config.muhurat_trading_dates.clear();
        let cal = TradingCalendar::from_config(&config).unwrap();
        assert_eq!(cal.holiday_count(), 0);
        // Any weekday should be a trading day
        let monday = NaiveDate::from_ymd_opt(2026, 3, 9).unwrap();
        assert!(cal.is_trading_day(monday));
    }

    #[test]
    fn test_holidays_loaded() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // Republic Day 2026
        assert!(cal.is_holiday(NaiveDate::from_ymd_opt(2026, 1, 26).unwrap()));
        // Holi 2026
        assert!(cal.is_holiday(NaiveDate::from_ymd_opt(2026, 3, 3).unwrap()));
    }

    #[test]
    fn test_all_entries_returns_sorted() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        let entries = cal.all_entries();
        // 3 holidays + 1 muhurat = 4 entries
        assert_eq!(entries.len(), 4);
        // Should be sorted by date
        for window in entries.windows(2) {
            assert!(window[0].date <= window[1].date);
        }
        // Republic Day is first
        assert_eq!(entries[0].name, "Republic Day");
        assert!(!entries[0].is_muhurat);
        // Last should be Diwali muhurat
        assert_eq!(entries[3].name, "Diwali 2026");
        assert!(entries[3].is_muhurat);
    }
}
