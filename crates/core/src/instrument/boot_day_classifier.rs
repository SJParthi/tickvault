//! Sub-PR #11b of 2026-05-27 daily-universe expansion — classify the
//! boot day (regular trading / weekend / declared holiday / Muhurat).
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Why this exists — honest layered defense (§22 + operator Q&A)
//!
//! Operator verified 2026-05-27 that there is no Dhan API for
//! holidays + the NSE official holiday-calendar URL is unverified.
//! Sub-PR #11b ships the layered defense WITHOUT a cross-check URL;
//! Sub-PR #11c will add the cross-check once the URL is verified.
//!
//! The runtime decision (is today a trading day?) uses ONLY the
//! operator-maintained `config/base.toml::trading.nse_holidays` list +
//! `[trading.muhurat_sessions]` table. The build-time staleness ratchet
//! + boot-time sanity check + annual reminder enforce that the config
//! is kept current.
//!
//! ## Decision matrix
//!
//! | Day classification | Boot action |
//! |---|---|
//! | `RegularTradingDay` | Full fetch + parse + subscribe (Sub-PR #10 chain) |
//! | `Weekend` | Skip subscribe; single low-key fetch attempt for cache freshness |
//! | `DeclaredHoliday` | Same as `Weekend` — no subscribe |
//! | `MuhuratTradingDay` | Special evening session — boot wakes at 17:30 IST (1hr before 18:30 session start), not 08:30 |

#![cfg(feature = "daily_universe_fetcher")]

use chrono::{Datelike, NaiveDate, Weekday};

/// Classification of today's boot day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDayClassification {
    /// Regular trading day Mon-Fri, not in the NSE holiday list.
    RegularTradingDay,
    /// Saturday or Sunday.
    Weekend,
    /// In the operator-maintained `nse_holidays` list.
    DeclaredHoliday,
    /// In the operator-maintained `muhurat_sessions` list — special
    /// evening session day (Diwali Laxmi Pujan).
    MuhuratTradingDay,
}

impl BootDayClassification {
    /// Stable wire-format string for the L5 AUDIT layer + CloudWatch
    /// metric dimensions.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RegularTradingDay => "regular_trading_day",
            Self::Weekend => "weekend",
            Self::DeclaredHoliday => "declared_holiday",
            Self::MuhuratTradingDay => "muhurat_trading_day",
        }
    }

    /// True if WebSocket subscription dispatch should happen today.
    /// Used by Sub-PR #10b's main.rs boot orchestrator.
    #[must_use]
    pub fn should_subscribe(self) -> bool {
        matches!(self, Self::RegularTradingDay | Self::MuhuratTradingDay)
    }
}

/// Result of the staleness sanity check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalenessOutcome {
    /// Config has enough future holidays — operator is up-to-date.
    Fresh,
    /// Config is stale — annual update required. Boot logs WARN
    /// (does NOT halt; the orchestrator decides whether to escalate).
    StaleNeedsUpdate {
        days_since_last_holiday: i64,
        last_holiday: NaiveDate,
    },
}

/// Minimum number of FUTURE holidays the config must contain to be
/// considered "fresh" enough for boot. Per the layered defense — if
/// fewer than this many remain, the operator hasn't updated the
/// config for the new year yet.
pub const MIN_FUTURE_HOLIDAYS_FOR_FRESH_CONFIG: usize = 3;

/// Classify today's boot day from operator-maintained config.
///
/// **Pure function** — no I/O, no clock reads. Caller passes today's
/// IST `NaiveDate` + the holiday + Muhurat lists.
///
/// # Performance
///
/// COLD PATH — called once at boot. Linear scan of holiday + Muhurat
/// lists (typically 14 + 1 entries). ~1µs.
#[must_use]
pub fn classify_boot_day(
    today_ist: NaiveDate,
    nse_holidays: &[NaiveDate],
    muhurat_session_dates: &[NaiveDate],
) -> BootDayClassification {
    // Muhurat takes priority over holiday classification (Diwali day
    // can also be a declared holiday for the morning session, but the
    // evening Muhurat session still trades).
    if muhurat_session_dates.contains(&today_ist) {
        return BootDayClassification::MuhuratTradingDay;
    }

    if nse_holidays.contains(&today_ist) {
        return BootDayClassification::DeclaredHoliday;
    }

    match today_ist.weekday() {
        Weekday::Sat | Weekday::Sun => BootDayClassification::Weekend,
        _ => BootDayClassification::RegularTradingDay,
    }
}

/// Boot-time staleness sanity check.
///
/// Counts how many holidays in the config are >= today. If fewer than
/// `MIN_FUTURE_HOLIDAYS_FOR_FRESH_CONFIG`, the config is stale —
/// operator must update from the latest NSE circular.
///
/// **Pure function.**
#[must_use]
pub fn check_holiday_config_staleness(
    today_ist: NaiveDate,
    nse_holidays: &[NaiveDate],
) -> StalenessOutcome {
    let future_count = nse_holidays.iter().filter(|d| **d >= today_ist).count();
    if future_count >= MIN_FUTURE_HOLIDAYS_FOR_FRESH_CONFIG {
        return StalenessOutcome::Fresh;
    }

    let last_holiday = nse_holidays.iter().copied().max().unwrap_or(NaiveDate::MIN);
    let days_since_last_holiday = today_ist.signed_duration_since(last_holiday).num_days();
    StalenessOutcome::StaleNeedsUpdate {
        days_since_last_holiday,
        last_holiday,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn typical_2026_holidays() -> Vec<NaiveDate> {
        // Placeholder — operator will replace from NSE 2026 circular.
        vec![
            date(2026, 1, 26),  // Republic Day
            date(2026, 3, 14),  // Holi (placeholder)
            date(2026, 4, 3),   // Good Friday (placeholder)
            date(2026, 4, 14),  // Mahavir Jayanti (placeholder)
            date(2026, 5, 1),   // May Day (placeholder)
            date(2026, 8, 15),  // Independence Day
            date(2026, 10, 2),  // Gandhi Jayanti
            date(2026, 11, 9),  // Diwali Laxmi Pujan day (placeholder)
            date(2026, 12, 25), // Christmas
        ]
    }

    #[test]
    fn regular_weekday_with_no_holiday_classifies_as_trading() {
        let today = date(2026, 6, 17); // Wednesday
        let result = classify_boot_day(today, &typical_2026_holidays(), &[]);
        assert_eq!(result, BootDayClassification::RegularTradingDay);
    }

    #[test]
    fn saturday_classifies_as_weekend() {
        let today = date(2026, 6, 20); // Saturday
        let result = classify_boot_day(today, &typical_2026_holidays(), &[]);
        assert_eq!(result, BootDayClassification::Weekend);
    }

    #[test]
    fn sunday_classifies_as_weekend() {
        let today = date(2026, 6, 21); // Sunday
        let result = classify_boot_day(today, &typical_2026_holidays(), &[]);
        assert_eq!(result, BootDayClassification::Weekend);
    }

    #[test]
    fn republic_day_classifies_as_declared_holiday() {
        let today = date(2026, 1, 26); // Monday — Republic Day
        let result = classify_boot_day(today, &typical_2026_holidays(), &[]);
        assert_eq!(result, BootDayClassification::DeclaredHoliday);
    }

    #[test]
    fn weekend_holiday_still_classifies_as_declared_holiday() {
        // Defensive — operator config might list a Saturday holiday.
        // Treat declared-holiday with higher precedence than weekend
        // for observability clarity.
        let holidays = vec![date(2026, 8, 15)]; // Aug 15 2026 = Saturday
        let today = date(2026, 8, 15);
        let result = classify_boot_day(today, &holidays, &[]);
        assert_eq!(result, BootDayClassification::DeclaredHoliday);
    }

    #[test]
    fn muhurat_date_takes_priority_over_holiday() {
        // Diwali Laxmi Pujan day MAY also appear in the holiday list
        // (morning session closed). The Muhurat classification wins.
        let muhurat = vec![date(2026, 11, 9)];
        let today = date(2026, 11, 9);
        let result = classify_boot_day(today, &typical_2026_holidays(), &muhurat);
        assert_eq!(result, BootDayClassification::MuhuratTradingDay);
    }

    #[test]
    fn muhurat_date_takes_priority_over_weekend() {
        // If Muhurat day falls on a Sunday (rare).
        let muhurat = vec![date(2026, 11, 8)]; // Nov 8 2026 = Sunday
        let today = date(2026, 11, 8);
        let result = classify_boot_day(today, &[], &muhurat);
        assert_eq!(result, BootDayClassification::MuhuratTradingDay);
    }

    #[test]
    fn should_subscribe_true_for_trading_and_muhurat_only() {
        assert!(BootDayClassification::RegularTradingDay.should_subscribe());
        assert!(BootDayClassification::MuhuratTradingDay.should_subscribe());
        assert!(!BootDayClassification::Weekend.should_subscribe());
        assert!(!BootDayClassification::DeclaredHoliday.should_subscribe());
    }

    #[test]
    fn as_str_returns_stable_wire_format() {
        assert_eq!(
            BootDayClassification::RegularTradingDay.as_str(),
            "regular_trading_day"
        );
        assert_eq!(BootDayClassification::Weekend.as_str(), "weekend");
        assert_eq!(
            BootDayClassification::DeclaredHoliday.as_str(),
            "declared_holiday"
        );
        assert_eq!(
            BootDayClassification::MuhuratTradingDay.as_str(),
            "muhurat_trading_day"
        );
    }

    // ---------- Staleness check tests ----------

    #[test]
    fn config_with_many_future_holidays_is_fresh() {
        let today = date(2026, 6, 1);
        let holidays = typical_2026_holidays(); // 9 entries, many after Jun 1
        let result = check_holiday_config_staleness(today, &holidays);
        assert_eq!(result, StalenessOutcome::Fresh);
    }

    #[test]
    fn config_with_zero_future_holidays_is_stale() {
        // Operator forgot to update for 2027 — today is Dec 26 2026,
        // last holiday in config is Dec 25 2026.
        let today = date(2026, 12, 26);
        let holidays = vec![date(2026, 12, 25)];
        let result = check_holiday_config_staleness(today, &holidays);
        match result {
            StalenessOutcome::StaleNeedsUpdate {
                last_holiday,
                days_since_last_holiday,
            } => {
                assert_eq!(last_holiday, date(2026, 12, 25));
                assert_eq!(days_since_last_holiday, 1);
            }
            StalenessOutcome::Fresh => panic!("expected stale"),
        }
    }

    #[test]
    fn config_with_exactly_min_future_holidays_is_fresh() {
        let today = date(2026, 10, 1);
        // 3 future holidays after Oct 1 — exactly at threshold.
        let holidays = vec![date(2026, 10, 2), date(2026, 11, 9), date(2026, 12, 25)];
        let result = check_holiday_config_staleness(today, &holidays);
        assert_eq!(result, StalenessOutcome::Fresh);
    }

    #[test]
    fn config_with_one_less_than_min_future_holidays_is_stale() {
        let today = date(2026, 11, 10);
        // Only 1 future holiday after Nov 10 — below threshold.
        let holidays = vec![date(2026, 11, 9), date(2026, 12, 25)];
        let result = check_holiday_config_staleness(today, &holidays);
        assert!(matches!(result, StalenessOutcome::StaleNeedsUpdate { .. }));
    }

    #[test]
    fn empty_holiday_config_is_stale() {
        let today = date(2026, 6, 1);
        let result = check_holiday_config_staleness(today, &[]);
        assert!(matches!(result, StalenessOutcome::StaleNeedsUpdate { .. }));
    }

    #[test]
    fn min_future_holidays_constant_is_three() {
        assert_eq!(MIN_FUTURE_HOLIDAYS_FOR_FRESH_CONFIG, 3);
    }

    #[test]
    fn deterministic_same_input_same_output() {
        let today = date(2026, 6, 17);
        let holidays = typical_2026_holidays();
        let muhurat = vec![date(2026, 11, 9)];
        let r1 = classify_boot_day(today, &holidays, &muhurat);
        let r2 = classify_boot_day(today, &holidays, &muhurat);
        assert_eq!(r1, r2);
    }
}
