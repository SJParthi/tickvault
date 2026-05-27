//! Sub-PR #11d of 2026-05-27 daily-universe expansion — extended boot
//! day classifier covering NSE late-added holidays + Sunday Budget
//! sessions + Muhurat timing-pending status.
//!
//! **Feature-gated.** Compiles only when `daily_universe_fetcher` is
//! enabled (per §21).
//!
//! ## Why a separate "extended" module
//!
//! The original Sub-PR #11b `boot_day_classifier` shipped a 3-input
//! classifier (`nse_holidays`, `muhurat_session_dates`). The operator-
//! verified NSE 2026 research file (docs/operator/nse-trading-calendar-2026.md)
//! revealed two real-world edge cases the 3-input design CANNOT model:
//!
//! 1. **Late-added holidays** — Jan 15 2026 was added MID-YEAR via a
//!    separate NSE circular for the Mumbai civic election. Operator
//!    config needs a separate `additional_holidays` list so the
//!    Dec 2025 annual circular remains untouched while late-add days
//!    accumulate.
//!
//! 2. **Extra trading days** — Feb 1 2026 (Sunday) ran a normal
//!    trading session for the Union Budget presentation. The
//!    weekday-only classifier would mark this as `Weekend` and skip
//!    subscription — wrong.
//!
//! 3. **Muhurat timing pending** — 2026-11-08 Diwali Laxmi Pujan is
//!    confirmed as a Muhurat day, but timings are deferred to
//!    Oct 2026. The classifier must distinguish `Verified` from
//!    `PendingTimingCircular` so the boot orchestrator refuses
//!    auto-orders on the pending case.
//!
//! Sub-PR #11d adds an `extended_classify_boot_day()` that supersedes
//! the basic classifier when the operator provides the additional
//! config lists. The basic `classify_boot_day()` from Sub-PR #11b is
//! kept for backward compatibility.

#![cfg(feature = "daily_universe_fetcher")]

use chrono::{Datelike, NaiveDate, Weekday};

use super::boot_day_classifier::BootDayClassification;

/// Muhurat session timing verification status.
///
/// Per the NSE 2026 verified research, the Dec 2025 annual circular
/// confirms the Muhurat DATE but defers TIMINGS to a separate Oct 2026
/// circular. The classifier MUST distinguish these so:
/// - `Verified` → boot orchestrator runs the orderly Muhurat session
/// - `PendingTimingCircular` → boot orchestrator refuses auto-orders
///   on this day; logs CRITICAL Telegram reminder to fetch the timing
///   circular.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuhuratStatus {
    /// Both date AND timings are operator-verified from an NSE circular.
    Verified,
    /// Date confirmed by NSE; timings deferred to a later NSE circular.
    /// Boot orchestrator MUST refuse auto-orders until updated.
    PendingTimingCircular,
}

impl MuhuratStatus {
    /// Stable wire-format label for the L5 AUDIT layer.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::PendingTimingCircular => "pending_timing_circular",
        }
    }

    /// Whether the boot orchestrator may auto-place orders on this day.
    /// `PendingTimingCircular` is a HALT-class status — operator must
    /// update the timing config before orders fire.
    #[must_use]
    pub fn allows_auto_orders(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// A Muhurat session entry — date + verification status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuhuratSession {
    pub date: NaiveDate,
    pub status: MuhuratStatus,
}

/// Errors that can occur during boot-day classification when the
/// operator config violates a sanity invariant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassifyConfigError {
    /// Same date appears in both `official_holidays` and
    /// `additional_holidays`. Likely a config-merge typo.
    #[error("date {date} appears in both official_holidays and additional_holidays")]
    DuplicateHolidayListing { date: NaiveDate },

    /// `extra_trading_days` contains a date that is ALSO in either
    /// `official_holidays` or `additional_holidays`. Logical
    /// contradiction — can't be both a holiday AND an extra trading
    /// day.
    #[error(
        "date {date} appears in extra_trading_days AND in a holiday list — config contradicts itself"
    )]
    ExtraTradingDayAlsoHoliday { date: NaiveDate },

    /// `muhurat_session_dates` contains a date that is ALSO in
    /// `extra_trading_days`. Possible but unusual — flag for operator.
    /// (e.g. Sunday Muhurat is fine in muhurat list; Sunday Budget is
    /// fine in extra_trading_days; the same date being BOTH is a typo.)
    #[error("date {date} appears in muhurat_session_dates AND extra_trading_days — pick one")]
    MuhuratAlsoExtraTradingDay { date: NaiveDate },
}

/// Extended classifier consuming all 4 operator-config lists.
///
/// **Pure function.** Priority ordering (highest wins):
/// 1. `Muhurat` (date in `muhurat_session_dates`)
/// 2. `DeclaredHoliday` (date in `official_holidays` OR `additional_holidays`)
/// 3. `RegularTradingDay` (date in `extra_trading_days` — overrides Sat/Sun)
/// 4. `Weekend` (Sat/Sun, not in any list)
/// 5. `RegularTradingDay` (default — weekday, not in any list)
///
/// # Errors
///
/// See [`ClassifyConfigError`] — fired on config sanity violations,
/// not on regular operation.
///
/// # Performance
///
/// COLD PATH — called once at boot. ~1µs over typical config lists
/// (15 official + 1-2 additional + 0-1 extra-trading + 0-1 muhurat).
pub fn extended_classify_boot_day(
    today_ist: NaiveDate,
    official_holidays: &[NaiveDate],
    additional_holidays: &[NaiveDate],
    extra_trading_days: &[NaiveDate],
    muhurat_sessions: &[MuhuratSession],
) -> Result<BootDayClassification, ClassifyConfigError> {
    validate_config_sanity(
        official_holidays,
        additional_holidays,
        extra_trading_days,
        muhurat_sessions,
    )?;

    // Priority 1: Muhurat
    if muhurat_sessions.iter().any(|m| m.date == today_ist) {
        return Ok(BootDayClassification::MuhuratTradingDay);
    }

    // Priority 2: declared holiday (either list)
    if official_holidays.contains(&today_ist) || additional_holidays.contains(&today_ist) {
        return Ok(BootDayClassification::DeclaredHoliday);
    }

    // Priority 3: extra trading day — overrides weekend
    if extra_trading_days.contains(&today_ist) {
        return Ok(BootDayClassification::RegularTradingDay);
    }

    // Priority 4-5: weekend vs regular
    Ok(match today_ist.weekday() {
        Weekday::Sat | Weekday::Sun => BootDayClassification::Weekend,
        _ => BootDayClassification::RegularTradingDay,
    })
}

/// Look up the Muhurat status for today, if today is a Muhurat day.
///
/// **Pure function.** Returns `None` if today is not in
/// `muhurat_sessions`. Returns `Some(status)` otherwise — caller uses
/// the status to gate auto-orders.
#[must_use]
pub fn muhurat_status_for_today(
    today_ist: NaiveDate,
    muhurat_sessions: &[MuhuratSession],
) -> Option<MuhuratStatus> {
    muhurat_sessions
        .iter()
        .find(|m| m.date == today_ist)
        .map(|m| m.status)
}

fn validate_config_sanity(
    official_holidays: &[NaiveDate],
    additional_holidays: &[NaiveDate],
    extra_trading_days: &[NaiveDate],
    muhurat_sessions: &[MuhuratSession],
) -> Result<(), ClassifyConfigError> {
    // Check 1: official vs additional duplicates
    for date in additional_holidays {
        if official_holidays.contains(date) {
            return Err(ClassifyConfigError::DuplicateHolidayListing { date: *date });
        }
    }

    // Check 2: extra_trading_days vs any holiday list
    for date in extra_trading_days {
        if official_holidays.contains(date) || additional_holidays.contains(date) {
            return Err(ClassifyConfigError::ExtraTradingDayAlsoHoliday { date: *date });
        }
    }

    // Check 3: muhurat vs extra_trading_days
    for muhurat in muhurat_sessions {
        if extra_trading_days.contains(&muhurat.date) {
            return Err(ClassifyConfigError::MuhuratAlsoExtraTradingDay { date: muhurat.date });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    /// The verified NSE 2026 official holiday list (15 weekday entries
    /// from NSE/CMTR/71775).
    fn nse_2026_official_holidays() -> Vec<NaiveDate> {
        vec![
            date(2026, 1, 26),  // Republic Day
            date(2026, 3, 3),   // Holi
            date(2026, 3, 26),  // Shri Ram Navami
            date(2026, 3, 31),  // Shri Mahavir Jayanti
            date(2026, 4, 3),   // Good Friday
            date(2026, 4, 14),  // Dr. Baba Saheb Ambedkar Jayanti
            date(2026, 5, 1),   // Maharashtra Day
            date(2026, 5, 28),  // Bakri Id
            date(2026, 6, 26),  // Muharram
            date(2026, 9, 14),  // Ganesh Chaturthi
            date(2026, 10, 2),  // Mahatma Gandhi Jayanti
            date(2026, 10, 20), // Dussehra
            date(2026, 11, 10), // Diwali-Balipratipada
            date(2026, 11, 24), // Prakash Gurpurb Sri Guru Nanak Dev
            date(2026, 12, 25), // Christmas
        ]
    }

    fn nse_2026_additional_holidays() -> Vec<NaiveDate> {
        vec![
            date(2026, 1, 15), // Mumbai civic election (added mid-year)
        ]
    }

    fn nse_2026_extra_trading_days() -> Vec<NaiveDate> {
        vec![
            date(2026, 2, 1), // Union Budget special session (Sunday)
        ]
    }

    fn nse_2026_muhurat() -> Vec<MuhuratSession> {
        vec![MuhuratSession {
            date: date(2026, 11, 8), // Diwali Laxmi Pujan (Sunday)
            status: MuhuratStatus::PendingTimingCircular,
        }]
    }

    // ---------- MuhuratStatus tests ----------

    #[test]
    fn muhurat_status_as_str_stable_wire_format() {
        assert_eq!(MuhuratStatus::Verified.as_str(), "verified");
        assert_eq!(
            MuhuratStatus::PendingTimingCircular.as_str(),
            "pending_timing_circular"
        );
    }

    #[test]
    fn allows_auto_orders_only_when_verified() {
        assert!(MuhuratStatus::Verified.allows_auto_orders());
        assert!(!MuhuratStatus::PendingTimingCircular.allows_auto_orders());
    }

    // ---------- Edge case 1: late-added holiday (Jan 15 2026) ----------

    #[test]
    fn jan_15_2026_civic_election_classifies_as_declared_holiday() {
        // Jan 15 2026 = Thursday — late-added via separate NSE circular.
        // Operator puts it in `additional_holidays`. Classifier must
        // treat it as DeclaredHoliday, not RegularTradingDay.
        let result = extended_classify_boot_day(
            date(2026, 1, 15),
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        )
        .expect("valid config");
        assert_eq!(result, BootDayClassification::DeclaredHoliday);
    }

    // ---------- Edge case 2: Sunday Budget trading (Feb 1 2026) ----------

    #[test]
    fn feb_1_2026_sunday_budget_classifies_as_regular_trading_day() {
        // Feb 1 2026 = Sunday — Union Budget special session.
        // Operator puts it in `extra_trading_days`. Classifier must
        // OVERRIDE the weekend default and return RegularTradingDay.
        let result = extended_classify_boot_day(
            date(2026, 2, 1),
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        )
        .expect("valid config");
        assert_eq!(result, BootDayClassification::RegularTradingDay);
    }

    // ---------- Edge case 3: Muhurat with pending timing ----------

    #[test]
    fn nov_8_2026_muhurat_classifies_correctly_with_pending_status() {
        // Nov 8 2026 = Sunday Muhurat with timings UNVERIFIED.
        // Classifier returns MuhuratTradingDay.
        // Separate muhurat_status_for_today() query returns
        // PendingTimingCircular so the boot orchestrator refuses
        // auto-orders.
        let today = date(2026, 11, 8);
        let result = extended_classify_boot_day(
            today,
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        )
        .expect("valid config");
        assert_eq!(result, BootDayClassification::MuhuratTradingDay);

        let status = muhurat_status_for_today(today, &nse_2026_muhurat()).expect("muhurat");
        assert_eq!(status, MuhuratStatus::PendingTimingCircular);
        assert!(!status.allows_auto_orders());
    }

    #[test]
    fn muhurat_status_returns_none_for_non_muhurat_day() {
        assert_eq!(
            muhurat_status_for_today(date(2026, 6, 17), &nse_2026_muhurat()),
            None
        );
    }

    // ---------- Regular cases ----------

    #[test]
    fn regular_weekday_with_no_holiday_classifies_as_trading() {
        let result = extended_classify_boot_day(
            date(2026, 6, 17), // Wednesday
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        )
        .expect("valid config");
        assert_eq!(result, BootDayClassification::RegularTradingDay);
    }

    #[test]
    fn republic_day_2026_classifies_as_declared_holiday() {
        let result = extended_classify_boot_day(
            date(2026, 1, 26), // Republic Day (Monday)
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        )
        .expect("valid config");
        assert_eq!(result, BootDayClassification::DeclaredHoliday);
    }

    #[test]
    fn regular_sunday_classifies_as_weekend() {
        // Jun 7 2026 = Sunday, NOT in extra_trading_days.
        let result = extended_classify_boot_day(
            date(2026, 6, 7),
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        )
        .expect("valid config");
        assert_eq!(result, BootDayClassification::Weekend);
    }

    // ---------- Priority ordering ----------

    #[test]
    fn muhurat_takes_priority_over_holiday_listing() {
        // Synthetic: same date in both muhurat list AND additional_holidays.
        // (Real NSE: Nov 8 2026 is Muhurat but Nov 10 2026 is the
        // "Diwali-Balipratipada" holiday — both are tracked separately.)
        let muhurat = vec![MuhuratSession {
            date: date(2026, 11, 8),
            status: MuhuratStatus::Verified,
        }];
        let additional = vec![date(2026, 11, 8)]; // hypothetical config error
        // Even with the duplicate, Muhurat wins.
        // The validate_config_sanity check ALLOWS this (muhurat date in
        // additional_holidays is explicitly OK — Diwali day may have
        // morning closure + evening Muhurat). The conflict is between
        // muhurat and extra_trading_days, not muhurat and holidays.
        let result = extended_classify_boot_day(date(2026, 11, 8), &[], &additional, &[], &muhurat)
            .expect("muhurat-in-holiday-list is allowed");
        assert_eq!(result, BootDayClassification::MuhuratTradingDay);
    }

    #[test]
    fn holiday_takes_priority_over_extra_trading_day() {
        // Validates via the config-sanity error path — declaring the
        // same date in BOTH holidays and extra_trading_days is a
        // config typo.
        let result = extended_classify_boot_day(
            date(2026, 1, 26),
            &[date(2026, 1, 26)],
            &[],
            &[date(2026, 1, 26)], // contradicts the holiday
            &[],
        );
        assert!(matches!(
            result,
            Err(ClassifyConfigError::ExtraTradingDayAlsoHoliday { .. })
        ));
    }

    #[test]
    fn extra_trading_day_overrides_weekend() {
        // Sunday Budget special — extra_trading_days wins over default
        // weekend classification.
        let result = extended_classify_boot_day(
            date(2026, 2, 1), // Sunday
            &[],
            &[],
            &[date(2026, 2, 1)],
            &[],
        )
        .expect("valid");
        assert_eq!(result, BootDayClassification::RegularTradingDay);
    }

    // ---------- Config sanity errors ----------

    #[test]
    fn rejects_duplicate_listing_in_both_holiday_arrays() {
        let result = extended_classify_boot_day(
            date(2026, 6, 17),
            &[date(2026, 1, 15)],
            &[date(2026, 1, 15)], // same as official — config typo
            &[],
            &[],
        );
        assert!(matches!(
            result,
            Err(ClassifyConfigError::DuplicateHolidayListing { .. })
        ));
    }

    #[test]
    fn rejects_extra_trading_day_that_is_also_additional_holiday() {
        let result = extended_classify_boot_day(
            date(2026, 6, 17),
            &[],
            &[date(2026, 2, 1)],
            &[date(2026, 2, 1)], // contradicts
            &[],
        );
        assert!(matches!(
            result,
            Err(ClassifyConfigError::ExtraTradingDayAlsoHoliday { .. })
        ));
    }

    #[test]
    fn rejects_muhurat_listed_as_extra_trading_day() {
        let muhurat = vec![MuhuratSession {
            date: date(2026, 11, 8),
            status: MuhuratStatus::PendingTimingCircular,
        }];
        let result = extended_classify_boot_day(
            date(2026, 6, 17),
            &[],
            &[],
            &[date(2026, 11, 8)], // contradicts the muhurat
            &muhurat,
        );
        assert!(matches!(
            result,
            Err(ClassifyConfigError::MuhuratAlsoExtraTradingDay { .. })
        ));
    }

    // ---------- Deterministic + empty-config ----------

    #[test]
    fn empty_config_classifies_weekday_as_trading() {
        let result =
            extended_classify_boot_day(date(2026, 6, 17), &[], &[], &[], &[]).expect("valid");
        assert_eq!(result, BootDayClassification::RegularTradingDay);
    }

    #[test]
    fn empty_config_classifies_sunday_as_weekend() {
        let result =
            extended_classify_boot_day(date(2026, 6, 21), &[], &[], &[], &[]).expect("valid");
        assert_eq!(result, BootDayClassification::Weekend);
    }

    #[test]
    fn deterministic_same_inputs_same_output() {
        let today = date(2026, 1, 15);
        let r1 = extended_classify_boot_day(
            today,
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        );
        let r2 = extended_classify_boot_day(
            today,
            &nse_2026_official_holidays(),
            &nse_2026_additional_holidays(),
            &nse_2026_extra_trading_days(),
            &nse_2026_muhurat(),
        );
        assert_eq!(r1, r2);
    }
}
