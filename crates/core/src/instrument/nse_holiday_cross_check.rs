//! Sub-PR #11c of 2026-05-27 daily-universe expansion — NSE holiday
//! URL cross-check helpers.
//!
//! **Feature-gated.** Compiles only when `daily_universe_fetcher` is
//! enabled (per §21).
//!
//! ## Purpose
//!
//! Per the operator-verified NSE 2026 research file
//! (`docs/operator/nse-trading-calendar-2026.md`), the NSE official
//! holiday list is exposed via an UNDOCUMENTED JSON endpoint:
//!
//! ```text
//! GET https://www.nseindia.com/api/holiday-master?type=trading
//! ```
//!
//! This module ships PURE-FUNCTION helpers to:
//! 1. Parse the Dhan/NSE date format (`DD-MMM-YYYY`, e.g.
//!    `"26-Jan-2026"`).
//! 2. Compare a fetched NSE date set against the operator's config
//!    `nse_holidays` list — returns a `HolidayDiscrepancy` enum.
//!
//! ## What this module does NOT do
//!
//! The actual HTTP fetch + cookie-warmup ritual is DEFERRED to
//! Sub-PR #10b's boot orchestrator. That's where the operator-config
//! flag `nse_holiday_cross_check_enabled` is read + the reqwest client
//! is constructed.
//!
//! The cookie-warmup ritual (per the operator research file §1c):
//!
//! ```ignore
//! // BEST-EFFORT cross-check — failure does NOT block boot.
//! let s = reqwest::Client::builder()
//!     .cookie_store(true)
//!     .user_agent("Mozilla/5.0 ...")
//!     .build()?;
//! s.get("https://www.nseindia.com").send().await?;          // warmup 1
//! s.get("https://www.nseindia.com/option-chain").send().await?; // warmup 2
//! let data: Value = s.get(DHAN_NSE_HOLIDAY_MASTER_URL).send().await?.json().await?;
//! ```
//!
//! ## Trust hierarchy (per operator research §0)
//!
//! 1. **Operator config TOML** — SOURCE OF TRUTH (immutable runtime
//!    decision)
//! 2. **NSE official PDF circular** — primary reference for operator
//!    to update config from
//! 3. **NSE HTML page** — secondary visual check
//! 4. **This undocumented JSON endpoint** — best-effort runtime cross-
//!    check ONLY. Do NOT block boot on it.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashSet;

use chrono::NaiveDate;
use tickvault_common::sanitize::sanitize_audit_string;

// Re-export the URL constant for crate consumers + tests.
pub use tickvault_common::constants::DHAN_NSE_HOLIDAY_CROSS_CHECK_URL as NSE_HOLIDAY_MASTER_URL;

/// Result of comparing operator config dates against NSE fetched
/// dates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolidayDiscrepancy {
    /// Config and NSE list match exactly. Operator is up-to-date.
    Match,

    /// NSE has dates that are NOT in operator config. Operator should
    /// review + update `additional_holidays`.
    OperatorMissingDates(Vec<NaiveDate>),

    /// Operator config has dates that NSE does NOT (yet) confirm.
    /// Could be:
    /// - Operator-added `additional_holidays` from a separate NSE
    ///   circular not yet in the JSON endpoint
    /// - Operator typo
    /// - NSE rolled back a holiday (rare)
    NseMissingDates(Vec<NaiveDate>),

    /// Both directions have mismatches. Most common in mid-year when
    /// the JSON endpoint is stale relative to operator additions.
    BothMissing {
        operator_missing: Vec<NaiveDate>,
        nse_missing: Vec<NaiveDate>,
    },
}

impl HolidayDiscrepancy {
    /// Stable wire-format label for L5 AUDIT + CloudWatch.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::OperatorMissingDates(_) => "operator_missing",
            Self::NseMissingDates(_) => "nse_missing",
            Self::BothMissing { .. } => "both_missing",
        }
    }

    /// True if there is ANY mismatch (operator should review).
    #[must_use]
    pub fn has_discrepancy(&self) -> bool {
        !matches!(self, Self::Match)
    }
}

/// Errors parsing the Dhan/NSE `DD-MMM-YYYY` date format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DateParseError {
    /// Input did not match the expected `DD-MMM-YYYY` shape (3 dash-
    /// separated parts).
    #[error("expected DD-MMM-YYYY format, got: {input}")]
    UnexpectedShape { input: String },

    /// Month abbreviation wasn't one of `Jan..Dec`.
    #[error("unknown month abbreviation: {month}")]
    UnknownMonth { month: String },

    /// Day / year couldn't parse as integer OR resulting date is
    /// invalid (e.g., Feb 30).
    #[error("invalid date components in: {input}")]
    InvalidDate { input: String },
}

/// Parse the Dhan/NSE date format `DD-MMM-YYYY` into a `NaiveDate`.
///
/// Examples:
/// - `"26-Jan-2026"` -> `NaiveDate(2026-01-26)`
/// - `"08-Nov-2026"` -> `NaiveDate(2026-11-08)`
///
/// **Pure function.** Defensive: sanitizes input via
/// `sanitize_audit_string` before parsing (strips control chars +
/// BiDi unicode that could appear in a malicious NSE response).
///
/// # Errors
///
/// See [`DateParseError`] variants.
pub fn parse_dd_mmm_yyyy(input: &str) -> Result<NaiveDate, DateParseError> {
    let cleaned = sanitize_audit_string(input.trim());

    let parts: Vec<&str> = cleaned.split('-').collect();
    if parts.len() != 3 {
        return Err(DateParseError::UnexpectedShape { input: cleaned });
    }

    let day: u32 = parts[0].parse().map_err(|_| DateParseError::InvalidDate {
        input: cleaned.clone(),
    })?;

    let month = month_abbrev_to_number(parts[1]).ok_or_else(|| DateParseError::UnknownMonth {
        month: parts[1].to_string(),
    })?;

    let year: i32 = parts[2].parse().map_err(|_| DateParseError::InvalidDate {
        input: cleaned.clone(),
    })?;

    NaiveDate::from_ymd_opt(year, month, day).ok_or(DateParseError::InvalidDate { input: cleaned })
}

#[must_use]
fn month_abbrev_to_number(abbrev: &str) -> Option<u32> {
    match abbrev.to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

/// Compare operator config dates vs NSE fetched dates.
///
/// **Pure function.** Caller passes both lists; this module classifies
/// the result. Failures of the actual fetch are handled by the caller
/// (best-effort — caller defaults to `Match` semantics on fetch
/// failure to avoid blocking boot).
#[must_use]
pub fn compare_holiday_sets(
    config_dates: &[NaiveDate],
    nse_dates: &[NaiveDate],
) -> HolidayDiscrepancy {
    let config_set: HashSet<NaiveDate> = config_dates.iter().copied().collect();
    let nse_set: HashSet<NaiveDate> = nse_dates.iter().copied().collect();

    let mut operator_missing: Vec<NaiveDate> = nse_set.difference(&config_set).copied().collect();
    operator_missing.sort();

    let mut nse_missing: Vec<NaiveDate> = config_set.difference(&nse_set).copied().collect();
    nse_missing.sort();

    match (operator_missing.is_empty(), nse_missing.is_empty()) {
        (true, true) => HolidayDiscrepancy::Match,
        (false, true) => HolidayDiscrepancy::OperatorMissingDates(operator_missing),
        (true, false) => HolidayDiscrepancy::NseMissingDates(nse_missing),
        (false, false) => HolidayDiscrepancy::BothMissing {
            operator_missing,
            nse_missing,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    // ---------- URL constant ----------

    #[test]
    fn nse_holiday_master_url_matches_research_file() {
        assert_eq!(
            NSE_HOLIDAY_MASTER_URL,
            "https://www.nseindia.com/api/holiday-master?type=trading"
        );
    }

    // ---------- DD-MMM-YYYY parser ----------

    #[test]
    fn parses_republic_day_format() {
        let result = parse_dd_mmm_yyyy("26-Jan-2026").expect("valid");
        assert_eq!(result, date(2026, 1, 26));
    }

    #[test]
    fn parses_diwali_muhurat_2026_date() {
        let result = parse_dd_mmm_yyyy("08-Nov-2026").expect("valid");
        assert_eq!(result, date(2026, 11, 8));
    }

    #[test]
    fn parses_all_12_months() {
        let cases = [
            ("01-Jan-2026", 1),
            ("01-Feb-2026", 2),
            ("01-Mar-2026", 3),
            ("01-Apr-2026", 4),
            ("01-May-2026", 5),
            ("01-Jun-2026", 6),
            ("01-Jul-2026", 7),
            ("01-Aug-2026", 8),
            ("01-Sep-2026", 9),
            ("01-Oct-2026", 10),
            ("01-Nov-2026", 11),
            ("01-Dec-2026", 12),
        ];
        for (input, expected_month) in cases {
            let result = parse_dd_mmm_yyyy(input).expect("valid");
            assert_eq!(
                result.format("%m").to_string().parse::<u32>().unwrap(),
                expected_month
            );
        }
    }

    #[test]
    fn parser_is_case_insensitive_for_month_abbrev() {
        assert_eq!(parse_dd_mmm_yyyy("26-jan-2026").unwrap(), date(2026, 1, 26));
        assert_eq!(parse_dd_mmm_yyyy("26-JAN-2026").unwrap(), date(2026, 1, 26));
        assert_eq!(parse_dd_mmm_yyyy("26-Jan-2026").unwrap(), date(2026, 1, 26));
    }

    #[test]
    fn parser_trims_whitespace() {
        assert_eq!(
            parse_dd_mmm_yyyy("  26-Jan-2026  ").unwrap(),
            date(2026, 1, 26)
        );
    }

    #[test]
    fn parser_rejects_wrong_shape() {
        // Slash separator → only 1 dash-separated part.
        let err = parse_dd_mmm_yyyy("26/Jan/2026").unwrap_err();
        assert!(matches!(err, DateParseError::UnexpectedShape { .. }));

        // 4 dash-separated parts.
        let err = parse_dd_mmm_yyyy("26-Jan-2026-extra").unwrap_err();
        assert!(matches!(err, DateParseError::UnexpectedShape { .. }));

        // 2 dash-separated parts.
        let err = parse_dd_mmm_yyyy("26-Jan").unwrap_err();
        assert!(matches!(err, DateParseError::UnexpectedShape { .. }));
    }

    #[test]
    fn parser_rejects_iso_format_as_unknown_month() {
        // ISO `2026-01-26` is 3 dash-parts but the middle is `01` not
        // a 3-letter month abbrev → UnknownMonth, NOT UnexpectedShape.
        let err = parse_dd_mmm_yyyy("2026-01-26").unwrap_err();
        assert!(matches!(err, DateParseError::UnknownMonth { .. }));
    }

    #[test]
    fn parser_rejects_unknown_month() {
        let err = parse_dd_mmm_yyyy("01-Foo-2026").unwrap_err();
        assert!(matches!(err, DateParseError::UnknownMonth { .. }));
    }

    #[test]
    fn parser_rejects_invalid_date() {
        // Feb 30 doesn't exist.
        let err = parse_dd_mmm_yyyy("30-Feb-2026").unwrap_err();
        assert!(matches!(err, DateParseError::InvalidDate { .. }));
    }

    #[test]
    fn parser_rejects_non_numeric_day() {
        let err = parse_dd_mmm_yyyy("XX-Jan-2026").unwrap_err();
        assert!(matches!(err, DateParseError::InvalidDate { .. }));
    }

    // ---------- Comparator ----------

    #[test]
    fn compare_with_identical_sets_returns_match() {
        let dates = vec![date(2026, 1, 26), date(2026, 3, 3)];
        let result = compare_holiday_sets(&dates, &dates);
        assert_eq!(result, HolidayDiscrepancy::Match);
        assert!(!result.has_discrepancy());
    }

    #[test]
    fn compare_with_empty_sets_returns_match() {
        let result = compare_holiday_sets(&[], &[]);
        assert_eq!(result, HolidayDiscrepancy::Match);
    }

    #[test]
    fn nse_has_more_dates_than_operator_returns_operator_missing() {
        let config = vec![date(2026, 1, 26)];
        let nse = vec![date(2026, 1, 26), date(2026, 3, 3)];
        let result = compare_holiday_sets(&config, &nse);
        match result {
            HolidayDiscrepancy::OperatorMissingDates(dates) => {
                assert_eq!(dates, vec![date(2026, 3, 3)]);
            }
            _ => panic!("expected OperatorMissingDates, got {result:?}"),
        }
    }

    #[test]
    fn operator_has_more_dates_than_nse_returns_nse_missing() {
        // Common case — operator adds Jan 15 (civic election) from
        // separate NSE circular; the JSON endpoint hasn't reflected it.
        let config = vec![date(2026, 1, 15), date(2026, 1, 26)];
        let nse = vec![date(2026, 1, 26)];
        let result = compare_holiday_sets(&config, &nse);
        match result {
            HolidayDiscrepancy::NseMissingDates(dates) => {
                assert_eq!(dates, vec![date(2026, 1, 15)]);
            }
            _ => panic!("expected NseMissingDates, got {result:?}"),
        }
    }

    #[test]
    fn both_directions_mismatched_returns_both_missing() {
        let config = vec![date(2026, 1, 15), date(2026, 1, 26)];
        let nse = vec![date(2026, 1, 26), date(2026, 3, 3)];
        let result = compare_holiday_sets(&config, &nse);
        match result {
            HolidayDiscrepancy::BothMissing {
                operator_missing,
                nse_missing,
            } => {
                assert_eq!(operator_missing, vec![date(2026, 3, 3)]);
                assert_eq!(nse_missing, vec![date(2026, 1, 15)]);
            }
            _ => panic!("expected BothMissing, got {result:?}"),
        }
    }

    #[test]
    fn comparator_handles_duplicate_dates_in_input() {
        // Deduped via HashSet — duplicates collapse silently.
        let config = vec![date(2026, 1, 26), date(2026, 1, 26)];
        let nse = vec![date(2026, 1, 26)];
        let result = compare_holiday_sets(&config, &nse);
        assert_eq!(result, HolidayDiscrepancy::Match);
    }

    #[test]
    fn comparator_sorts_missing_dates_ascending() {
        let config = vec![date(2026, 12, 25)];
        let nse = vec![
            date(2026, 3, 3),
            date(2026, 1, 26),
            date(2026, 12, 25),
            date(2026, 5, 1),
        ];
        let result = compare_holiday_sets(&config, &nse);
        match result {
            HolidayDiscrepancy::OperatorMissingDates(dates) => {
                assert_eq!(
                    dates,
                    vec![date(2026, 1, 26), date(2026, 3, 3), date(2026, 5, 1)]
                );
            }
            _ => panic!("expected OperatorMissingDates"),
        }
    }

    #[test]
    fn discrepancy_as_str_stable_wire_format() {
        assert_eq!(HolidayDiscrepancy::Match.as_str(), "match");
        assert_eq!(
            HolidayDiscrepancy::OperatorMissingDates(vec![date(2026, 1, 26)]).as_str(),
            "operator_missing"
        );
        assert_eq!(
            HolidayDiscrepancy::NseMissingDates(vec![date(2026, 1, 15)]).as_str(),
            "nse_missing"
        );
        assert_eq!(
            HolidayDiscrepancy::BothMissing {
                operator_missing: vec![],
                nse_missing: vec![]
            }
            .as_str(),
            "both_missing"
        );
    }

    #[test]
    fn has_discrepancy_truth_table() {
        assert!(!HolidayDiscrepancy::Match.has_discrepancy());
        assert!(
            HolidayDiscrepancy::OperatorMissingDates(vec![date(2026, 1, 26)]).has_discrepancy()
        );
        assert!(HolidayDiscrepancy::NseMissingDates(vec![date(2026, 1, 26)]).has_discrepancy());
        assert!(
            HolidayDiscrepancy::BothMissing {
                operator_missing: vec![date(2026, 1, 1)],
                nse_missing: vec![],
            }
            .has_discrepancy()
        );
    }

    #[test]
    fn deterministic_pure_function() {
        let config = vec![date(2026, 1, 26), date(2026, 3, 3)];
        let nse = vec![date(2026, 3, 3), date(2026, 5, 1)];
        let r1 = compare_holiday_sets(&config, &nse);
        let r2 = compare_holiday_sets(&config, &nse);
        assert_eq!(r1, r2);
    }
}
