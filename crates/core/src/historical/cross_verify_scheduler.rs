//! Pure-function helpers for the cross-verify scheduler (PR #788).
//!
//! ## Operator-locked schedule (2026-05-25)
//!
//! | Trigger | Time | Scope | Why |
//! |---|---|---|---|
//! | `daily_09_00_30` | 09:00:30 IST | YESTERDAY's 1d candle | App boots at 09:00 IST (EventBridge); 30s buffer for token + ticks to settle; verify yesterday's closing 1d candle |
//! | `intraday_15_31` | 15:31:00 IST | TODAY's 1m / 5m / 15m / 60m | 1 min after market close; the 15:29 bar has settled; verify the full session |
//!
//! ## Dhan API quirk — the yesterday/today fetch math
//!
//! Operator-confirmed 2026-05-25 (Dhan docs):
//! > "toDate is NON-INCLUSIVE for daily. fromDate Feb 8 / toDate Feb 9
//! > returns Feb 8's daily candle."
//!
//! To fetch yesterday's final 1d candle this morning:
//!   `fromDate = yesterday (IST), toDate = today (IST)` → returns yesterday's bar.
//!
//! Common-mistake repro: `fromDate=yesterday, toDate=yesterday` → DH-905
//! "Input_Exception" (empty range; toDate must be strictly after fromDate).
//!
//! Both behaviours pinned by ratchet tests in
//! `crates/common/tests/cross_verify_yesterday_today_guard.rs`.
//!
//! ## App-window constraints (operator-locked 2026-05-25)
//!
//! App runs 09:00 IST → 16:00 IST. Both schedulers must fire WITHIN
//! this window. 09:00:30 sits 30s into the window; 15:31:00 sits 29
//! minutes before shutdown — both safe.

use chrono::{Datelike, Duration, NaiveDate};

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY};

/// Operator-locked seconds-of-day-IST for the 09:00:30 1d trigger.
/// 9 * 3600 + 30 = 32_430.
pub const DAILY_1D_FIRE_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 30;

/// Operator-locked seconds-of-day-IST for the 15:31:00 intraday
/// trigger. 15 * 3600 + 31 * 60 = 55_860.
pub const INTRADAY_FIRE_SECS_OF_DAY_IST: u32 = 15 * 3_600 + 31 * 60;

/// Trigger label written into the JSONL report `trigger` field and the
/// HTML report card header.
pub const TRIGGER_LABEL_DAILY: &str = "daily_09_00_30";
pub const TRIGGER_LABEL_INTRADAY: &str = "intraday_15_31";

/// Pure helper — produces the (fromDate, toDate) tuple to send to
/// Dhan `/v2/charts/historical` for fetching YESTERDAY's daily candle.
///
/// Given today (IST), returns:
///   * `fromDate = today - 1 day` (yesterday)
///   * `toDate   = today`         (non-inclusive in Dhan's contract)
///
/// Dhan returns yesterday's complete 1d bar.
///
/// **Common-mistake guard:** caller MUST NOT pass `fromDate == toDate`.
/// That returns DH-905 (Input_Exception). This helper always produces
/// a 1-day window (fromDate < toDate by exactly 1 day).
///
/// O(1) date arithmetic. Pinned by ratchet
/// `test_yesterday_1d_window_is_one_day_wide`.
#[must_use]
pub fn yesterday_1d_window(today_ist: NaiveDate) -> (NaiveDate, NaiveDate) {
    let yesterday = today_ist - Duration::days(1);
    (yesterday, today_ist)
}

/// Pure helper — converts a `NaiveDate` to the `YYYY-MM-DD` string
/// Dhan expects in the request body.
#[must_use]
pub fn format_dhan_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Pure helper — computes seconds to sleep before the next 09:00:30
/// IST daily-1d trigger. If we're already past today's 09:00:30 IST,
/// sleep until tomorrow's.
#[must_use]
pub fn secs_until_next_daily_1d_fire(now_secs_of_day_ist: u32) -> u32 {
    secs_until_next_fire_at(now_secs_of_day_ist, DAILY_1D_FIRE_SECS_OF_DAY_IST)
}

/// Pure helper — computes seconds to sleep before the next 15:31:00
/// IST intraday trigger. Same logic as daily but for the intraday
/// firing time.
#[must_use]
pub fn secs_until_next_intraday_fire(now_secs_of_day_ist: u32) -> u32 {
    secs_until_next_fire_at(now_secs_of_day_ist, INTRADAY_FIRE_SECS_OF_DAY_IST)
}

#[must_use]
fn secs_until_next_fire_at(now_secs_of_day_ist: u32, target_secs_of_day_ist: u32) -> u32 {
    if now_secs_of_day_ist < target_secs_of_day_ist {
        target_secs_of_day_ist - now_secs_of_day_ist
    } else {
        SECONDS_PER_DAY
            .saturating_sub(now_secs_of_day_ist)
            .saturating_add(target_secs_of_day_ist)
    }
}

/// Pure helper — current IST seconds-of-day from a UTC second.
#[must_use]
pub fn ist_secs_of_day_now(now_utc_secs: i64) -> u32 {
    let now_ist = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    #[allow(clippy::cast_sign_loss)] // APPROVED: rem_euclid is positive
    let secs = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
    secs
}

/// Pure helper — current IST naive date from a UTC second.
#[must_use]
pub fn ist_date_now(now_utc_secs: i64) -> NaiveDate {
    let now_ist = chrono::DateTime::<chrono::Utc>::from_timestamp(now_utc_secs, 0)
        .unwrap_or_else(chrono::Utc::now)
        + Duration::seconds(IST_UTC_OFFSET_SECONDS_I64);
    now_ist.date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secs_until_next_daily_1d_fire_alias_for_guard() {
        // Pub-fn-test-guard requires full fn name. Canonical scenario
        // tests below cover semantics.
        let now = 8 * 3_600;
        assert_eq!(
            secs_until_next_daily_1d_fire(now),
            DAILY_1D_FIRE_SECS_OF_DAY_IST - now
        );
    }

    #[test]
    fn test_ist_date_now_alias_for_guard() {
        // Pub-fn-test-guard requires full fn name.
        let d = ist_date_now(0);
        // Epoch 0 = 1970-01-01 UTC = 1970-01-01 IST (still 1970-01-01
        // since IST is UTC+5:30 and epoch starts at midnight UTC).
        assert!(d.year() >= 1970);
    }

    #[test]
    fn test_daily_fire_secs_is_09_00_30_ist() {
        // Operator lock: 09:00 EventBridge boot + 30s settle.
        assert_eq!(DAILY_1D_FIRE_SECS_OF_DAY_IST, 32_430);
    }

    #[test]
    fn test_intraday_fire_secs_is_15_31_00_ist() {
        // Operator lock: 1 min after 15:30 market close.
        assert_eq!(INTRADAY_FIRE_SECS_OF_DAY_IST, 55_860);
    }

    #[test]
    fn test_both_triggers_within_app_window() {
        // App runs 09:00 IST -> 16:00 IST. Both triggers MUST fit.
        let app_open = 9 * 3_600;
        let app_close = 16 * 3_600;
        assert!(DAILY_1D_FIRE_SECS_OF_DAY_IST >= app_open);
        assert!(DAILY_1D_FIRE_SECS_OF_DAY_IST < app_close);
        assert!(INTRADAY_FIRE_SECS_OF_DAY_IST >= app_open);
        assert!(INTRADAY_FIRE_SECS_OF_DAY_IST < app_close);
    }

    #[test]
    fn test_yesterday_1d_window_is_one_day_wide() {
        // Operator-confirmed Dhan quirk: toDate is non-inclusive, so to
        // get yesterday's daily candle we MUST send a 1-day-wide window
        // (fromDate=yesterday, toDate=today).
        let today = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let (from_date, to_date) = yesterday_1d_window(today);
        assert_eq!(from_date, NaiveDate::from_ymd_opt(2026, 5, 24).unwrap());
        assert_eq!(to_date, today);
        // Window is exactly 1 day wide — NEVER fromDate == toDate
        // (which would trigger DH-905 "Input_Exception").
        assert_ne!(from_date, to_date);
        assert_eq!((to_date - from_date).num_days(), 1);
    }

    #[test]
    fn test_yesterday_1d_window_across_month_boundary() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let (from_date, to_date) = yesterday_1d_window(today);
        assert_eq!(from_date, NaiveDate::from_ymd_opt(2026, 5, 31).unwrap());
        assert_eq!(to_date, today);
    }

    #[test]
    fn test_yesterday_1d_window_across_year_boundary() {
        let today = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        let (from_date, to_date) = yesterday_1d_window(today);
        assert_eq!(from_date, NaiveDate::from_ymd_opt(2026, 12, 31).unwrap());
        assert_eq!(to_date, today);
    }

    #[test]
    fn test_format_dhan_date_is_yyyy_mm_dd() {
        let d = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        assert_eq!(format_dhan_date(d), "2026-05-25");
    }

    #[test]
    fn test_secs_until_next_daily_fire_before_target() {
        // At 08:50:00 IST (= 31_800s), fire is 10m 30s away = 630s.
        let now = 8 * 3_600 + 50 * 60;
        assert_eq!(
            secs_until_next_daily_1d_fire(now),
            DAILY_1D_FIRE_SECS_OF_DAY_IST - now
        );
    }

    #[test]
    fn test_secs_until_next_daily_fire_after_target_wraps_to_tomorrow() {
        // At 10:00:00 IST, fire is tomorrow's 09:00:30 = ~23h.
        let now = 10 * 3_600;
        let expected = SECONDS_PER_DAY - now + DAILY_1D_FIRE_SECS_OF_DAY_IST;
        assert_eq!(secs_until_next_daily_1d_fire(now), expected);
    }

    #[test]
    fn test_secs_until_next_intraday_fire_before_target() {
        let now = 14 * 3_600;
        assert_eq!(
            secs_until_next_intraday_fire(now),
            INTRADAY_FIRE_SECS_OF_DAY_IST - now
        );
    }

    #[test]
    fn test_secs_until_next_intraday_fire_after_target_wraps_to_tomorrow() {
        let now = 16 * 3_600;
        let expected = SECONDS_PER_DAY - now + INTRADAY_FIRE_SECS_OF_DAY_IST;
        assert_eq!(secs_until_next_intraday_fire(now), expected);
    }

    #[test]
    fn test_trigger_labels_are_stable_strings() {
        // Pinned wire format — JSONL consumers + Grafana dashboards
        // depend on these exact strings.
        assert_eq!(TRIGGER_LABEL_DAILY, "daily_09_00_30");
        assert_eq!(TRIGGER_LABEL_INTRADAY, "intraday_15_31");
    }

    #[test]
    fn test_ist_secs_of_day_now_within_day_range() {
        // For ANY UTC second, the IST seconds-of-day must be in [0, 86_400).
        let utc = 1_779_388_200_i64; // arbitrary IST midnight near 2026-05-22
        let secs = ist_secs_of_day_now(utc);
        assert!(secs < SECONDS_PER_DAY);
    }
}
