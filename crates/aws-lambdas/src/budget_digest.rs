//! Daily budget digest — Rust port of the `budget-guards.tf` inline legacy
//! heredoc (`tv-prod-daily-budget-digest`, phase 2b-1).
//!
//! Runs on an EventBridge cron Mon-Fri. Reads month-to-date spend from the
//! free AWS Budgets API (`budgets:DescribeBudget`,
//! `Budget.CalculatedSpend.ActualSpend`) and publishes the Telegram-formatted
//! digest to SNS `tv-prod-alerts`.
//!
//! **⚠ CHANGED 2026-10-22 (plan item 63, operator Quote 37: "free spend
//! lookups").** Until this change the digest made three paid Cost Explorer
//! `GetCostAndUsage` calls (yesterday, month-to-date, month-to-date by
//! service). `DescribeBudget` returns ONE month-to-date total, so the
//! "Yesterday" line and the "Where it goes" per-service lines are gone. The
//! Budgets figure is refreshed by AWS about every 8 to 12 hours (Assumed, AWS
//! docs) and carries no month stamp, so on UTC day 1 the digest may show the
//! month that just ended; the digest stops nothing, so it shows the figure
//! with that caveat line rather than hiding it (see `crate::budget_spend`).
//!
//! Environment variables: `ALERTS_TOPIC_ARN`, `BUDGET_NAME`,
//! `BUDGET_ACCOUNT_ID`.
//!
//! Parity notes kept: constants, emoji thresholds, date math (UTC dates), the
//! single-`$` USD renders, and the `{'ok': True, 'mtd_usd': .., 'pct': ..}`
//! return shape (plus `source`).

use chrono::{Datelike, Duration, NaiveDate, Utc};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::info;

/// Rupee display rate (what you actually pay incl GST).
pub const INR_PER_USD: f64 = 85.0;
/// India GST 18%.
pub const GST_MULT: f64 = 1.18;
/// The REAL ceiling is the AWS Budget that auto-stops the box (budget.tf
/// limit_amount) and the hourly `hard_stop_guard` kill line.
///
/// 2026-09-23 (Quote 23, operator): **$225 for September 2026 ONLY**, so the
/// month is not stopped at the old $150 line with ~a week of trading left;
/// from October 2026 the standing ceiling is **$150** again. The one-month
/// allowance is enforced in CODE, not by remembering to revert: the digest
/// and the guard both read
/// `hard_stop_guard::effective_budget_kill_usd(BUDGET_USD, year, month)`,
/// which clamps every month except 2026-09 to
/// `hard_stop_guard::STANDING_BUDGET_KILL_USD` ($170 since operator Quote 30, 2026-10-13; $100 from
/// November 2026 per operator Quote 33, then $85 per operator Quote 36, then $75 (GST included) from November 2026 per operator Quote 37,
/// 2026-10-21, with October 2026 capped at $170).
///
/// History: $55 → $25 (2026-07-19) → $35 (2026-07-31) → $100 (2026-08-08)
/// → $130 (2026-08-19) → $150 (2026-08-25, Quote 19) → $225 September only
/// → $150 (2026-10-01, audit PR30b: the scheduled revert; the month clamp now
/// changes nothing for any month, and stays as the backstop) → $170
/// (2026-10-13, operator Quote 30: AWS at or under ₹15,000 a month).
/// KEEP IN SYNC with budget.tf limit_amount + budget-guards.tf
/// BUDGET_KILL_USD + hard_stop_guard::DEFAULT_BUDGET_KILL_USD —
/// `budget_ceiling_lockstep_guard.rs` fails the build if they drift.
pub const BUDGET_USD: f64 = 170.0;

/// SNS subject — legacy parity: `'[BUDGET] daily AWS cost'`.
pub const DIGEST_SUBJECT: &str = "[BUDGET] daily AWS cost";

/// The caveat line shown on UTC day 1, when a Budgets figure may still be
/// last month's (a `DescribeBudget` reply carries no month stamp).
pub const FIRST_DAY_CAVEAT: &str =
    "_Note_: on the 1st this figure may still be last month's (AWS refreshes it every 8-12 h)";

/// USD → display rupees (incl GST).
pub fn inr(usd: f64) -> f64 {
    usd * INR_PER_USD * GST_MULT
}

/// Traffic-light emoji — legacy parity: 🟢 <50, 🟡 <80, 🟠 <100, 🔴 ≥100.
pub fn emoji_for_pct(pct: f64) -> &'static str {
    if pct < 50.0 {
        "🟢"
    } else if pct < 80.0 {
        "🟡"
    } else if pct < 100.0 {
        "🟠"
    } else {
        "🔴"
    }
}

/// Calendar math for the digest — legacy parity including the December
/// special case (`... if today_utc.month < 12 else 31`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonthMath {
    pub days_in_month: u32,
    pub days_remaining: u32,
    pub forecast_usd: f64,
}

pub fn month_math(today_utc: NaiveDate, mtd_usd: f64) -> MonthMath {
    let days_in_month = if today_utc.month() < 12 {
        NaiveDate::from_ymd_opt(today_utc.year(), today_utc.month() + 1, 1)
            .map(|first_of_next| (first_of_next - Duration::days(1)).day())
            .unwrap_or(31)
    } else {
        31
    };
    let days_remaining = days_in_month.saturating_sub(today_utc.day());
    // legacy: `(mtd_usd / today_utc.day) * days_in_month if today_utc.day
    // else mtd_usd` — day is never 0, the guard is kept for parity.
    let forecast_usd = if today_utc.day() > 0 {
        (mtd_usd / f64::from(today_utc.day())) * f64::from(days_in_month)
    } else {
        mtd_usd
    };
    MonthMath {
        days_in_month,
        days_remaining,
        forecast_usd,
    }
}

/// Render the Telegram digest body + the `% of budget`.
///
/// Since 2026-10-22 (plan item 63) the body has one spend line (this month,
/// from AWS Budgets); the "Yesterday" and per-service lines went with Cost
/// Explorer. On UTC day 1 a caveat line says the figure may be last month's.
pub fn render_digest(today_utc: NaiveDate, mtd_usd: f64) -> (String, f64) {
    // The ceiling THIS month — the same function the hourly guard uses, so
    // the digest and the kill line can never name different numbers.
    // `today_utc` is the UTC billing day (AWS billing's own calendar).
    let ceiling = crate::hard_stop_guard::effective_budget_kill_usd(
        BUDGET_USD,
        today_utc.year(),
        today_utc.month(),
    );
    let pct = if ceiling != 0.0 {
        (mtd_usd / ceiling) * 100.0
    } else {
        0.0
    };
    let emoji = emoji_for_pct(pct);
    let m = month_math(today_utc, mtd_usd);

    let mut lines = vec![
        format!("{emoji} *AWS Cost — tickvault*"),
        format!("_This month_:  ₹{:.0}   (${:.2})", inr(mtd_usd), mtd_usd),
        // Derived from the EFFECTIVE ceiling for this UTC month.
        format!("_Of ${ceiling:.0} stop-budget_: {pct:.0}%"),
        format!(
            "_Forecast EOM_: ₹{:.0}   (${:.2})",
            inr(m.forecast_usd),
            m.forecast_usd
        ),
        format!("_Days left_:   {}", m.days_remaining),
    ];
    if today_utc.day() == crate::budget_spend::UNSTAMPED_FIRST_DAY_OF_MONTH {
        lines.push(FIRST_DAY_CAVEAT.to_string());
    }
    (lines.join("\n"), pct)
}

/// The success return — legacy parity: `{'ok': True, 'mtd_usd': .., 'pct': ..}`,
/// plus the spend source (`budgets`).
pub fn success_result(mtd_usd: f64, pct: f64) -> Value {
    json!({
        "ok": true,
        "mtd_usd": mtd_usd,
        "pct": pct,
        "source": crate::budget_spend::SPEND_SOURCE,
    })
}

/// Entry point — EventBridge cron invoke (payload unused, like the legacy runtime).
///
/// UNPROVEN until deploy: the live Budgets + SNS legs run only in a real
/// Lambda invoke. A failed or refused Budgets read (missing reply pieces,
/// NaN, negative, non-USD) is an `Err`: the Lambda errors and its Errors
/// metric fires, never a silent ₹0 digest.
pub async fn handle(_event: Value) -> Result<Value, Error> {
    let topic_arn = std::env::var("ALERTS_TOPIC_ARN")
        .map_err(|_| Error::from("ALERTS_TOPIC_ARN env var is missing"))?;
    let coords = crate::budget_spend::budget_coordinates_from_env().map_err(Error::from)?;

    let today_utc = Utc::now().date_naive();
    let budgets = crate::clients::budgets_us_east_1().await;
    let mtd_usd = crate::budget_spend::describe_actual_spend(&budgets, &coords)
        .await
        .map_err(Error::from)?;

    let (message, pct) = render_digest(today_utc, mtd_usd);

    let config = crate::clients::sdk_config().await;
    let sns = crate::clients::sns(&config);
    sns.publish()
        .topic_arn(&topic_arn)
        .subject(DIGEST_SUBJECT)
        .message(&message)
        .send()
        .await?;
    info!(
        mtd_usd,
        pct,
        source = crate::budget_spend::SPEND_SOURCE,
        "daily budget digest published"
    );

    Ok(success_result(mtd_usd, pct))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures below are dated July 2026, so the digest names the
    /// STANDING ceiling, never the September-only allowance.
    fn july_ceiling() -> f64 {
        crate::hard_stop_guard::effective_budget_kill_usd(BUDGET_USD, 2026, 7)
    }

    #[test]
    fn test_digest_names_the_standing_line_in_every_month_since_the_revert() {
        // Operator Quote 30 (2026-10-13): the configured ceiling is $170, so
        // even a September-2026 date (the old one-month allowance) reads $170.
        let sept = NaiveDate::from_ymd_opt(2026, 9, 23).unwrap();
        let (msg, pct) = render_digest(sept, 85.0);
        assert!(msg.contains("_Of $170 stop-budget_: 50%"), "{msg}");
        assert!((pct - 50.0).abs() < 1e-9);

        // October: $170 of spend is 100% of the standing $170 line — red.
        let oct = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let (msg, pct) = render_digest(oct, 170.0);
        assert!(msg.contains("_Of $170 stop-budget_: 100%"), "{msg}");
        assert!(msg.starts_with("🔴"), "{msg}");
        assert!((pct - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_inr_applies_rate_and_gst() {
        assert!((inr(1.0) - 100.3).abs() < 1e-9); // 85 * 1.18
        assert_eq!(inr(0.0), 0.0);
    }

    #[test]
    fn test_emoji_thresholds_match_legacy() {
        assert_eq!(emoji_for_pct(0.0), "🟢");
        assert_eq!(emoji_for_pct(49.999), "🟢");
        assert_eq!(emoji_for_pct(50.0), "🟡");
        assert_eq!(emoji_for_pct(79.999), "🟡");
        assert_eq!(emoji_for_pct(80.0), "🟠");
        assert_eq!(emoji_for_pct(99.999), "🟠");
        assert_eq!(emoji_for_pct(100.0), "🔴");
        assert_eq!(emoji_for_pct(250.0), "🔴");
    }

    #[test]
    fn test_month_math_mid_month() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let m = month_math(d, 20.0);
        assert_eq!(m.days_in_month, 31);
        assert_eq!(m.days_remaining, 13);
        // (20 / 18) * 31
        assert!((m.forecast_usd - (20.0 / 18.0) * 31.0).abs() < 1e-9);
    }

    #[test]
    fn test_month_math_february_and_leap() {
        let feb = NaiveDate::from_ymd_opt(2026, 2, 10).unwrap();
        assert_eq!(month_math(feb, 1.0).days_in_month, 28);
        let leap = NaiveDate::from_ymd_opt(2028, 2, 10).unwrap();
        assert_eq!(month_math(leap, 1.0).days_in_month, 29);
    }

    #[test]
    fn test_month_math_december_legacy_special_case() {
        // legacy: `... if today_utc.month < 12 else 31`.
        let dec = NaiveDate::from_ymd_opt(2026, 12, 5).unwrap();
        let m = month_math(dec, 5.0);
        assert_eq!(m.days_in_month, 31);
        assert_eq!(m.days_remaining, 26);
    }

    #[test]
    fn test_render_digest_full_message_shape() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let (msg, pct) = render_digest(today, 10.0);
        let ceiling = july_ceiling();
        assert!((pct - (10.0 / ceiling) * 100.0).abs() < 1e-9);
        assert!(msg.starts_with("🟢 *AWS Cost — tickvault*"));
        assert!(msg.contains("_This month_:  ₹1003   ($10.00)"), "{msg}");
        // Both halves derived: the ceiling AND the percentage of it, so an
        // authorized budget change never turns into a spurious failure.
        let expected_pct = (10.00_f64 / ceiling * 100.0).round();
        assert!(
            msg.contains(&format!(
                "_Of ${ceiling:.0} stop-budget_: {expected_pct:.0}%"
            )),
            "{msg}"
        );
        assert!(msg.contains("_Days left_:   13"), "{msg}");
        assert!(!msg.contains(FIRST_DAY_CAVEAT), "{msg}");
    }

    #[test]
    fn test_render_digest_has_no_yesterday_or_per_service_lines() {
        // 2026-10-22 (plan item 63): DescribeBudget gives one total; the
        // lines that needed Cost Explorer are gone, never rendered as ₹0.
        let today = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let (msg, _) = render_digest(today, 10.0);
        assert!(!msg.contains("Yesterday"), "{msg}");
        assert!(!msg.contains("Where it goes"), "{msg}");
    }

    #[test]
    fn test_render_digest_first_utc_day_carries_the_caveat() {
        let day1 = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let (msg, _) = render_digest(day1, 160.0);
        assert!(msg.contains(FIRST_DAY_CAVEAT), "{msg}");
        let day2 = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let (msg, _) = render_digest(day2, 3.0);
        assert!(!msg.contains(FIRST_DAY_CAVEAT), "{msg}");
    }

    #[test]
    fn test_render_digest_empty_month() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 2).unwrap();
        let (msg, pct) = render_digest(today, 0.0);
        assert_eq!(pct, 0.0);
        assert!(msg.contains(&format!("_Of ${:.0} stop-budget_: 0%", july_ceiling())));
    }

    #[test]
    fn test_render_digest_over_budget_is_red() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        // Derived from the const so a future ceiling change can never
        // silently turn this into an under-budget case.
        let (msg, pct) = render_digest(today, july_ceiling() * 1.2);
        assert!(pct > 100.0);
        assert!(msg.starts_with("🔴"));
    }

    #[test]
    fn test_success_result_shape() {
        let v = success_result(12.5, 22.7);
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["mtd_usd"], serde_json::json!(12.5));
        assert_eq!(v["pct"], serde_json::json!(22.7));
        assert_eq!(v["source"], serde_json::json!("budgets"));
    }

    #[test]
    fn test_digest_subject_is_legacy_literal() {
        assert_eq!(DIGEST_SUBJECT, "[BUDGET] daily AWS cost");
    }
}
