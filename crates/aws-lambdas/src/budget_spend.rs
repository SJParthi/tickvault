//! Month-to-date spend from the free AWS Budgets API.
//!
//! ADDED 2026-10-22 (plan item 63, operator Quote 37: "free spend lookups").
//! The hard-stop guard, the daily budget digest and the operator console read
//! month-to-date spend from `budgets:DescribeBudget`
//! (`Budget.CalculatedSpend.ActualSpend`) on the one tickvault budget
//! (`aws_budgets_budget.tv_monthly`). Until this change they called paid Cost
//! Explorer `GetCostAndUsage` (billed per request); `DescribeBudget` is free.
//!
//! # Latency (stated, not hidden)
//!
//! AWS refreshes a budget's calculated spend about every 8 to 12 hours (AWS
//! Budgets documentation; Assumed, not measured here). Cost Explorer lagged
//! too (about a day for most services), so the guard was never real-time, but
//! the Budgets figure can trail the bill by up to half a day more. The
//! budget's own native stop action (`aws_budgets_budget_action.stop_at_100` in
//! `budget.tf`) is the second, independent stop: it is evaluated by AWS on the
//! same figure and does not depend on any Lambda here.
//!
//! # No month stamp
//!
//! A `DescribeBudget` reply carries no timestamp on `CalculatedSpend`, so in
//! the first 24 hours (UTC) of a billing month the figure may still be the
//! month that just ended. `trusted_spend_for_line` therefore treats a reading
//! at or above the stop line during UTC day 1 as UNKNOWN (page, never stop).
//! Cost Explorer already returned unknown for the whole of day 1 (its
//! `[1st, 1st)` interval is empty and the request failed), so this is no
//! weaker than before. A reading below the line on day 1 is used as is: it
//! can only under-state the new month by at most last month's tail, and it can
//! never stop the box.
//!
//! **⚠ CHANGED 2026-10-26 (audit PR L4) — the distrust now covers the first
//! 48 hours (UTC days 1 AND 2), not day 1 only.** AWS refreshes the figure
//! about every 8 to 12 hours and has been seen to lag longer, so on day 2 a
//! reading at or above the line may still be the month that just ended (on
//! Monday 2 November October's total could stop the box). A distrusted reading
//! is unknown exactly as on day 1: it pages and never stops. The native budget
//! stop is unaffected. Cost: a genuine breach on day 2 pages instead of
//! stopping, and the next run (day 3) stops it.
//!
//! # Refusals
//!
//! A reply with no budget, no calculated spend or no actual spend, an amount
//! that does not parse, a NaN or infinite amount, a negative amount, or a unit
//! other than USD is an error. Every caller maps an error to "spend unknown":
//! the guard pages and never stops, the digest Lambda fails loudly (its Errors
//! metric fires), the console shows the figure as unavailable. Before this
//! change the guard parsed `"NaN"` as a valid amount, and every `spend >= line`
//! compare with NaN is false, so a NaN reply could never stop the box
//! (fail-open); it is now refused with a coded `LAMBDA-AWS-01` error.
//!
//! Cold path: one request per invoke (hourly guard, daily digest, console
//! refresh). No hot-path constraint applies.

use chrono::{DateTime, Datelike, Utc};
use tracing::{error, warn};

/// Environment variable naming the budget (`aws_budgets_budget.tv_monthly.name`).
pub const BUDGET_NAME_ENV: &str = "BUDGET_NAME";
/// Environment variable naming the account that owns the budget
/// (`data.aws_caller_identity.current.account_id`).
pub const BUDGET_ACCOUNT_ID_ENV: &str = "BUDGET_ACCOUNT_ID";
/// The only spend unit accepted. The budget is created with `limit_unit = "USD"`.
pub const SPEND_UNIT_USD: &str = "USD";
/// The spend source named in logs and return values.
pub const SPEND_SOURCE: &str = "budgets";
/// The first UTC day of a billing month on which a Budgets figure may still be
/// the previous month's (see the module header).
pub const UNSTAMPED_FIRST_DAY_OF_MONTH: u32 = 1;
/// The last UTC day of that window (2026-10-26, audit PR L4: was day 1 only).
pub const UNSTAMPED_LAST_DAY_OF_MONTH: u32 = 2;

/// Which budget to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetCoordinates {
    pub account_id: String,
    pub budget_name: String,
}

/// Build the coordinates from the two raw values. Both must be present and
/// non-blank; a missing one is an error, never a guess.
pub fn budget_coordinates_from(
    account_id: Option<String>,
    budget_name: Option<String>,
) -> Result<BudgetCoordinates, String> {
    let account_id = account_id.map(|v| v.trim().to_string()).unwrap_or_default();
    let budget_name = budget_name
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    if account_id.is_empty() {
        return Err(format!(
            "{BUDGET_ACCOUNT_ID_ENV} env var is missing or empty"
        ));
    }
    if budget_name.is_empty() {
        return Err(format!("{BUDGET_NAME_ENV} env var is missing or empty"));
    }
    Ok(BudgetCoordinates {
        account_id,
        budget_name,
    })
}

// TEST-EXEMPT: thin env read over budget_coordinates_from, which is tested.
pub fn budget_coordinates_from_env() -> Result<BudgetCoordinates, String> {
    budget_coordinates_from(
        std::env::var(BUDGET_ACCOUNT_ID_ENV).ok(),
        std::env::var(BUDGET_NAME_ENV).ok(),
    )
}

/// Parse one `Spend { Amount, Unit }`. Refuses a unit other than USD, an
/// amount that does not parse, a NaN or infinite amount (coded
/// `LAMBDA-AWS-01`: it would make every stop compare false) and a negative
/// amount.
pub fn parse_spend_amount(amount: &str, unit: &str) -> Result<f64, String> {
    if unit.trim() != SPEND_UNIT_USD {
        return Err(format!(
            "Budgets spend unit {unit:?} is not {SPEND_UNIT_USD}"
        ));
    }
    let value = amount
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("bad Budgets spend amount {amount:?}: {e}"))?;
    if !value.is_finite() {
        error!(
            code = "LAMBDA-AWS-01",
            amount = %amount,
            "budgets:DescribeBudget returned a non-finite spend amount; refused as unknown spend"
        );
        return Err(format!("non-finite Budgets spend amount {amount:?}"));
    }
    if value < 0.0 {
        return Err(format!("negative Budgets spend amount {amount:?}"));
    }
    Ok(value)
}

/// Month-to-date actual spend (USD) from one `DescribeBudget` reply. A reply
/// missing the budget, its calculated spend or its actual spend is an error
/// (unknown spend), never a silent $0.
pub fn actual_spend_of(
    out: &aws_sdk_budgets::operation::describe_budget::DescribeBudgetOutput,
) -> Result<f64, String> {
    let budget = out
        .budget()
        .ok_or_else(|| "DescribeBudget reply has no Budget".to_string())?;
    let calculated = budget
        .calculated_spend()
        .ok_or_else(|| "DescribeBudget reply has no Budget.CalculatedSpend".to_string())?;
    let actual = calculated.actual_spend().ok_or_else(|| {
        "DescribeBudget reply has no Budget.CalculatedSpend.ActualSpend".to_string()
    })?;
    parse_spend_amount(actual.amount(), actual.unit())
}

/// True during the first 48 hours (UTC days 1 and 2) of a billing month, when
/// a Budgets figure may still be the previous month's.
pub fn in_unstamped_month_start(now_utc: DateTime<Utc>) -> bool {
    (UNSTAMPED_FIRST_DAY_OF_MONTH..=UNSTAMPED_LAST_DAY_OF_MONTH).contains(&now_utc.day())
}

/// The spend a stop decision may use. During UTC days 1 and 2 a reading at or above
/// `line_usd` is treated as unknown (`None`: page, never stop), because it may
/// be last month's total. Every other reading passes through unchanged.
pub fn trusted_spend_for_line(
    spend: Option<f64>,
    line_usd: f64,
    now_utc: DateTime<Utc>,
) -> Option<f64> {
    match spend {
        Some(v) if in_unstamped_month_start(now_utc) && v >= line_usd => {
            warn!(
                spend_usd = v,
                line_usd,
                source = SPEND_SOURCE,
                "first 48 h (UTC) of the month: a Budgets reading at or above the line may be last month's; treated as unknown"
            );
            None
        }
        other => other,
    }
}

// TEST-EXEMPT: live AWS call; its decision points are actual_spend_of + parse_spend_amount, both tested.
pub async fn describe_actual_spend(
    client: &aws_sdk_budgets::Client,
    coords: &BudgetCoordinates,
) -> Result<f64, String> {
    let out = client
        .describe_budget()
        .account_id(&coords.account_id)
        .budget_name(&coords.budget_name)
        .send()
        .await
        .map_err(|e| format!("budgets:DescribeBudget failed: {e}"))?;
    actual_spend_of(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_budgets::operation::describe_budget::DescribeBudgetOutput;
    use aws_sdk_budgets::types::{Budget, BudgetType, CalculatedSpend, Spend, TimeUnit};
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0)
            .single()
            .expect("valid test timestamp")
    }

    /// A recorded `DescribeBudget` body (shape from the AWS Budgets API
    /// reference, values from the tickvault budget), read field by field the
    /// way the SDK deserializer fills the output.
    const RECORDED_BODY: &str = r#"{
      "Budget": {
        "BudgetName": "tv-prod-monthly",
        "BudgetLimit": {"Amount": "170.0", "Unit": "USD"},
        "TimeUnit": "MONTHLY",
        "BudgetType": "COST",
        "CalculatedSpend": {
          "ActualSpend": {"Amount": "57.312", "Unit": "USD"},
          "ForecastedSpend": {"Amount": "121.4", "Unit": "USD"}
        }
      }
    }"#;

    fn output_with_spend(spend: Option<(&str, &str)>) -> DescribeBudgetOutput {
        let mut calculated = CalculatedSpend::builder();
        if let Some((amount, unit)) = spend {
            calculated = calculated.actual_spend(
                Spend::builder()
                    .amount(amount)
                    .unit(unit)
                    .build()
                    .expect("spend builds"),
            );
        }
        let budget = Budget::builder()
            .budget_name("tv-prod-monthly")
            .time_unit(TimeUnit::Monthly)
            .budget_type(BudgetType::Cost)
            .calculated_spend(calculated.build())
            .build()
            .expect("budget builds");
        DescribeBudgetOutput::builder().budget(budget).build()
    }

    fn output_from_recorded_body(body: &str) -> DescribeBudgetOutput {
        let v: serde_json::Value = serde_json::from_str(body).expect("recorded body is JSON");
        let actual = &v["Budget"]["CalculatedSpend"]["ActualSpend"];
        output_with_spend(Some((
            actual["Amount"].as_str().expect("Amount"),
            actual["Unit"].as_str().expect("Unit"),
        )))
    }

    #[test]
    fn test_actual_spend_of_reads_the_recorded_describe_budget_body() {
        let out = output_from_recorded_body(RECORDED_BODY);
        let got = actual_spend_of(&out).expect("spend");
        assert!((got - 57.312).abs() < 1e-9, "{got}");
    }

    #[test]
    fn test_actual_spend_of_zero_spend_reads_zero_not_a_failure() {
        // First hours of a month: AWS reports "0" — that is a reading.
        let out = output_with_spend(Some(("0", "USD")));
        assert_eq!(actual_spend_of(&out).expect("spend"), 0.0);
    }

    #[test]
    fn test_actual_spend_of_missing_pieces_are_errors() {
        assert!(actual_spend_of(&DescribeBudgetOutput::builder().build()).is_err());
        let no_actual = output_with_spend(None);
        let err = actual_spend_of(&no_actual).expect_err("missing ActualSpend");
        assert!(err.contains("ActualSpend"), "{err}");
        let budget_no_calc = Budget::builder()
            .budget_name("tv-prod-monthly")
            .time_unit(TimeUnit::Monthly)
            .budget_type(BudgetType::Cost)
            .build()
            .expect("budget builds");
        let out = DescribeBudgetOutput::builder()
            .budget(budget_no_calc)
            .build();
        let err = actual_spend_of(&out).expect_err("missing CalculatedSpend");
        assert!(err.contains("CalculatedSpend"), "{err}");
    }

    #[test]
    fn test_parse_spend_amount_refuses_nan_infinite_negative_garbage_and_other_units() {
        for bad in ["NaN", "nan", "inf", "-inf", "infinity", "", "abc", "-0.01"] {
            assert!(
                parse_spend_amount(bad, "USD").is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(parse_spend_amount("10", "INR").is_err());
        assert!(parse_spend_amount("10", "").is_err());
        assert!((parse_spend_amount(" 12.5 ", "USD").expect("ok") - 12.5).abs() < 1e-9);
        assert_eq!(parse_spend_amount("0", "USD").expect("ok"), 0.0);
    }

    #[test]
    fn test_actual_spend_of_nan_reply_is_refused() {
        // The old Cost Explorer parse accepted "NaN" (fail-open).
        assert!(actual_spend_of(&output_with_spend(Some(("NaN", "USD")))).is_err());
    }

    #[test]
    fn test_budget_coordinates_from_requires_both_values() {
        let ok = budget_coordinates_from(Some(" 123 ".into()), Some("tv-prod-monthly".into()))
            .expect("ok");
        assert_eq!(ok.account_id, "123");
        assert_eq!(ok.budget_name, "tv-prod-monthly");
        assert!(budget_coordinates_from(None, Some("b".into())).is_err());
        assert!(budget_coordinates_from(Some("1".into()), None).is_err());
        assert!(budget_coordinates_from(Some(" ".into()), Some("b".into())).is_err());
        assert!(budget_coordinates_from(Some("1".into()), Some("".into())).is_err());
    }

    #[test]
    fn test_in_unstamped_month_start_is_utc_days_one_and_two() {
        assert!(in_unstamped_month_start(utc(2026, 11, 1, 0)));
        assert!(in_unstamped_month_start(utc(2026, 11, 1, 23)));
        assert!(in_unstamped_month_start(utc(2026, 11, 2, 0)));
        assert!(in_unstamped_month_start(utc(2026, 11, 2, 23)));
        assert!(!in_unstamped_month_start(utc(2026, 11, 3, 0)));
        assert!(!in_unstamped_month_start(utc(2026, 10, 31, 23)));
    }

    #[test]
    fn test_trusted_spend_for_line_day_one_at_or_above_line_is_unknown() {
        let day1 = utc(2026, 11, 1, 4);
        assert_eq!(trusted_spend_for_line(Some(75.0), 75.0, day1), None);
        assert_eq!(trusted_spend_for_line(Some(160.0), 75.0, day1), None);
        // Below the line on day 1 passes through: it can never stop the box.
        assert_eq!(trusted_spend_for_line(Some(3.0), 75.0, day1), Some(3.0));
        assert_eq!(trusted_spend_for_line(None, 75.0, day1), None);
        // From day 3 a breach reading is trusted.
        let day3 = utc(2026, 11, 3, 0);
        assert_eq!(trusted_spend_for_line(Some(80.0), 75.0, day3), Some(80.0));
    }

    // Regression: 2026-10-26 (audit PR L4) — only UTC day 1 was distrusted, so
    // on Monday 2 November a Budgets figure still showing October's total at
    // or above the line could stop the box.
    #[test]
    fn test_regression_day_two_reading_at_or_above_line_is_unknown() {
        let day2 = utc(2026, 11, 2, 4);
        assert_eq!(trusted_spend_for_line(Some(160.0), 75.0, day2), None);
        assert_eq!(trusted_spend_for_line(Some(75.0), 75.0, day2), None);
        assert_eq!(trusted_spend_for_line(Some(3.0), 75.0, day2), Some(3.0));
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;
    use crate::test_support::{EnvVarGuard, env_lock};

    #[test]
    fn test_budget_coordinates_from_env_reads_both_variables() {
        let _lock = env_lock();
        let _acct = EnvVarGuard::set(BUDGET_ACCOUNT_ID_ENV, Some(" 123456789012 "));
        let _name = EnvVarGuard::set(BUDGET_NAME_ENV, Some("tv-prod-monthly"));
        assert_eq!(
            budget_coordinates_from_env(),
            Ok(BudgetCoordinates {
                account_id: "123456789012".to_string(),
                budget_name: "tv-prod-monthly".to_string(),
            })
        );
    }

    #[test]
    fn test_budget_coordinates_from_env_refuses_a_missing_variable() {
        let _lock = env_lock();
        let _acct = EnvVarGuard::set(BUDGET_ACCOUNT_ID_ENV, Some("123456789012"));
        let _name = EnvVarGuard::set(BUDGET_NAME_ENV, None);
        assert_eq!(
            budget_coordinates_from_env(),
            Err(format!("{BUDGET_NAME_ENV} env var is missing or empty"))
        );
    }
}
