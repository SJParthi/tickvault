//! The NSE trading calendar for the Lambdas (2026-11-07).
//!
//! The app reads its calendar from `config/base.toml`
//! `[[trading.nse_holidays]]`. The Lambdas do not ship that file, and until
//! today they knew only the weekday and the app's holiday-stop marker, which
//! the app writes AFTER it boots on a holiday. So a Lambda that ran before the
//! marker existed (or on a day the box never booted) could not tell a
//! weekday holiday from a trading day, and paged or redeployed on it.
//!
//! [`NSE_HOLIDAYS`] is the same date list, compiled in. It cannot drift from
//! `config/base.toml`: `tests/nse_calendar_lockstep_guard.rs` fails the build
//! when the two differ, so a calendar edit lands in both in the same change.
//! The Lambda bundle is rebuilt whenever `crates/aws-lambdas` changes (the
//! terraform apply hashes the crate), so the deployed list follows.
//!
//! Owner, 2026-11-07: "see today is saturday why the fuck server started dude
//! why?" and "I dont need any hallucination or illusion I just need the
//! working guaranteed assurance solution".
//!
//! **Muhurat.** A Muhurat session date is NOT a trading day here. It always
//! falls on a weekend or an NSE holiday, the box is started for it by the
//! start watchdog's dated `muhurat_check` (not by any weekday path), and every
//! workflow refuses to deploy on it (Quote 49). Callers that must act on a
//! Muhurat date check `muhurat_window` themselves.
//!
//! **Honest limit.** A date in a year the list does not cover reads as a
//! trading day when it is a weekday (the same fail-open the app's own
//! calendar has once its coverage runs out; the app's calendar-coverage
//! warning covers that).
//!
//! Pure functions only: O(holidays) per call (16 entries), no I/O.

use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};

/// NSE trading holidays (IST dates), exactly the `[[trading.nse_holidays]]`
/// dates in `config/base.toml`, in the same order.
pub const NSE_HOLIDAYS: &[&str] = &[
    "2026-01-15",
    "2026-01-26",
    "2026-03-03",
    "2026-03-26",
    "2026-03-31",
    "2026-04-03",
    "2026-04-14",
    "2026-05-01",
    "2026-05-28",
    "2026-06-26",
    "2026-09-14",
    "2026-10-02",
    "2026-10-20",
    "2026-11-10",
    "2026-11-24",
    "2026-12-25",
];

/// `true` when `date` is listed in [`NSE_HOLIDAYS`].
#[must_use]
pub fn is_nse_holiday(date: NaiveDate) -> bool {
    let wanted = date.format("%Y-%m-%d").to_string();
    NSE_HOLIDAYS.iter().any(|d| *d == wanted)
}

/// `true` on a regular NSE trading day: Monday to Friday and not an NSE
/// holiday. Weekends, holidays and Muhurat dates (always one of the two) are
/// `false`.
#[must_use]
pub fn is_nse_trading_day(date: NaiveDate) -> bool {
    !matches!(date.weekday(), Weekday::Sat | Weekday::Sun) && !is_nse_holiday(date)
}

/// The IST calendar date of `now_utc`.
#[must_use]
pub fn ist_date(now_utc: DateTime<Utc>) -> NaiveDate {
    (now_utc + chrono::Duration::seconds(i64::from(crate::time::IST_OFFSET_SECS))).date_naive()
}

/// [`is_nse_trading_day`] for the IST date of `now_utc`.
#[must_use]
pub fn is_nse_trading_day_at(now_utc: DateTime<Utc>) -> bool {
    is_nse_trading_day(ist_date(now_utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn test_is_nse_trading_day_and_is_nse_holiday_classify_upcoming_days() {
        // Sat 2026-11-07 (the day the owner asked), Sun 2026-11-08 Muhurat,
        // Mon 2026-11-09 trading, Tue 2026-11-10 NSE holiday.
        assert!(!is_nse_trading_day(d(2026, 11, 7)));
        assert!(!is_nse_trading_day(d(2026, 11, 8)));
        assert!(is_nse_trading_day(d(2026, 11, 9)));
        assert!(!is_nse_trading_day(d(2026, 11, 10)));
        assert!(is_nse_holiday(d(2026, 11, 10)));
        assert!(!is_nse_holiday(d(2026, 11, 9)));
    }

    #[test]
    fn test_every_listed_holiday_parses_and_is_not_a_trading_day() {
        for raw in NSE_HOLIDAYS {
            let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").expect("YYYY-MM-DD");
            assert!(is_nse_holiday(date), "{raw}");
            assert!(!is_nse_trading_day(date), "{raw}");
        }
    }

    #[test]
    fn test_ist_date_and_is_nse_trading_day_at_roll_over_at_1830_utc() {
        // Fri 2026-11-06 18:29:59 UTC is Fri 23:59:59 IST; one second later
        // it is Saturday in IST.
        let fri = Utc.with_ymd_and_hms(2026, 11, 6, 18, 29, 59).unwrap();
        assert!(is_nse_trading_day_at(fri));
        assert!(!is_nse_trading_day_at(fri + chrono::Duration::seconds(1)));
        // Mon 2026-11-09 03:00 UTC (08:30 IST): trading. Tue 03:00 UTC: holiday.
        assert!(is_nse_trading_day_at(
            Utc.with_ymd_and_hms(2026, 11, 9, 3, 0, 0).unwrap()
        ));
        assert!(!is_nse_trading_day_at(
            Utc.with_ymd_and_hms(2026, 11, 10, 3, 0, 0).unwrap()
        ));
    }

    #[test]
    fn test_uncovered_year_weekday_reads_as_trading() {
        // Honest limit pinned: the list covers 2026 only.
        assert!(is_nse_trading_day(d(2027, 1, 26)));
    }
}
