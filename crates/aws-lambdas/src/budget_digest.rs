//! Daily budget digest — Rust port of the `budget-guards.tf` inline Python
//! heredoc (`tv-prod-daily-budget-digest`, phase 2b-1).
//!
//! Runs 17:30 IST (12:00 UTC) Mon-Fri via EventBridge cron. Queries Cost
//! Explorer for yesterday's spend + month-to-date (+ per-service split) and
//! publishes the Telegram-formatted digest to SNS `tv-prod-alerts`.
//!
//! Environment variables (unchanged): `ALERTS_TOPIC_ARN`.
//!
//! Parity notes: constants, emoji thresholds, date math (UTC dates —
//! Cost Explorer's "end" is exclusive), the exact line format including the
//! `{:<16}` service column and the single-`$` USD renders (the heredoc's
//! `$$` was terraform escaping for a literal `$`), and the
//! `{'ok': True, 'mtd_usd': .., 'pct': ..}` return shape.

use chrono::{Datelike, Duration, NaiveDate, Utc};
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::info;

/// Rupee display rate (what you actually pay incl GST).
pub const INR_PER_USD: f64 = 85.0;
/// India GST 18%.
pub const GST_MULT: f64 = 1.18;
/// The REAL ceiling is the AWS Budget that auto-stops the box (budget.tf
/// limit_amount): $55/month on UnblendedCost. KEEP IN SYNC with budget.tf
/// limit_amount — the digest reads BUDGET_USD, the native Budget Action +
/// killswitch fire at limit_amount.
pub const BUDGET_USD: f64 = 55.0;

/// SNS subject — Python parity: `'[BUDGET] daily AWS cost'`.
pub const DIGEST_SUBJECT: &str = "[BUDGET] daily AWS cost";

/// Friendly labels for the Cost Explorer SERVICE dimension (substring match).
pub const SERVICE_LABELS: [(&str, &str); 8] = [
    ("Elastic Compute Cloud", "EC2 compute"),
    ("EC2 - Other", "EBS + transfer"),
    ("Virtual Private Cloud", "Public IP / VPC"),
    ("CloudWatch", "CloudWatch"),
    ("Simple Storage", "S3 storage"),
    ("Simple Notification", "SNS alerts"),
    ("Lambda", "Lambda"),
    ("Key Management", "KMS"),
];

/// Substring-match a raw SERVICE dimension onto its friendly label.
pub fn label_for(svc: &str) -> &str {
    for (needle, nice) in SERVICE_LABELS {
        if svc.contains(needle) {
            return nice;
        }
    }
    svc
}

/// USD → display rupees (incl GST).
pub fn inr(usd: f64) -> f64 {
    usd * INR_PER_USD * GST_MULT
}

/// Traffic-light emoji — Python parity: 🟢 <50, 🟡 <80, 🟠 <100, 🔴 ≥100.
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

/// Calendar math for the digest — Python parity including the December
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
    // Python: `(mtd_usd / today_utc.day) * days_in_month if today_utc.day
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

/// Fold raw `(SERVICE, usd)` groups into labeled aggregates — Python
/// parity: skip `usd <= 0`, aggregate on the friendly label, first-seen
/// insertion order (a Vec fold, not a hash map, so tie ordering is stable).
pub fn aggregate_by_service(groups: &[(String, f64)]) -> Vec<(String, f64)> {
    let mut agg: Vec<(String, f64)> = Vec::new();
    for (svc, usd) in groups {
        if *usd <= 0.0 {
            continue;
        }
        let key = label_for(svc);
        if let Some(entry) = agg.iter_mut().find(|(k, _)| k == key) {
            entry.1 += usd;
        } else {
            agg.push((key.to_string(), *usd));
        }
    }
    agg
}

/// Render the Telegram digest body + the `% of budget`.
///
/// Line-for-line parity with the Python heredoc (note the heredoc's `$$`
/// is terraform escaping — the deployed Python rendered a single `$`).
pub fn render_digest(
    today_utc: NaiveDate,
    yday_usd: f64,
    mtd_usd: f64,
    by_svc: &[(String, f64)],
) -> (String, f64) {
    let pct = if BUDGET_USD != 0.0 {
        (mtd_usd / BUDGET_USD) * 100.0
    } else {
        0.0
    };
    let emoji = emoji_for_pct(pct);
    let m = month_math(today_utc, mtd_usd);

    let mut lines = vec![
        format!("{emoji} *AWS Cost — tickvault*"),
        format!("_Yesterday_:   ₹{:.0}   (${:.2})", inr(yday_usd), yday_usd),
        format!("_This month_:  ₹{:.0}   (${:.2})", inr(mtd_usd), mtd_usd),
        format!("_Of $55 stop-budget_: {pct:.0}%"),
        format!(
            "_Forecast EOM_: ₹{:.0}   (${:.2})",
            inr(m.forecast_usd),
            m.forecast_usd
        ),
        format!("_Days left_:   {}", m.days_remaining),
        String::new(),
        "*Where it goes (this month):*".to_string(),
    ];

    let mut sorted: Vec<&(String, f64)> = by_svc.iter().collect();
    // Python: `sorted(..., key=lambda kv: -kv[1])` — descending by USD,
    // stable for ties (insertion order preserved).
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (nice, usd) in &sorted {
        lines.push(format!("  {nice:<16} ₹{:.0}  (${:.2})", inr(*usd), usd));
    }
    if by_svc.is_empty() {
        lines.push("  (no spend yet this month)".to_string());
    }
    (lines.join("\n"), pct)
}

/// Parse a Cost Explorer `Amount` string list into a summed f64 — Python
/// parity: `float(...)` raises on garbage, so a bad amount is an Err
/// (the Lambda errors, its Errors metric fires).
pub fn sum_amounts<'a, I: IntoIterator<Item = &'a str>>(amounts: I) -> Result<f64, Error> {
    let mut total = 0.0_f64;
    for a in amounts {
        total += a
            .parse::<f64>()
            .map_err(|e| Error::from(format!("bad Cost Explorer amount {a:?}: {e}")))?;
    }
    Ok(total)
}

/// Require a Cost Explorer response key — Python parity (hostile-review r1
/// F4): the heredoc's `d['Total']['UnblendedCost']['Amount']` /
/// `g['Keys'][0]` / `g['Metrics'][...]` chains RAISE KeyError/IndexError on
/// any missing key, so the Lambda errors and its Errors metric fires. A
/// silent skip here would render a false-OK ₹0 digest. Amount PARSE
/// failures stay loud separately (`sum_amounts` + the by-service parse).
pub fn require_key<T>(value: Option<T>, key: &str) -> Result<T, Error> {
    value.ok_or_else(|| {
        Error::from(format!(
            "Cost Explorer response missing {key:?} (python KeyError parity — never a silent $0 digest)"
        ))
    })
}

/// Pull a day's `Total.UnblendedCost.Amount` string — errors loudly on any
/// missing key (python KeyError parity, hostile-review r1 F4).
pub fn day_unblended_amount(
    day: &aws_sdk_costexplorer::types::ResultByTime,
) -> Result<&str, Error> {
    let total = require_key(day.total(), "Total")?;
    let metric = require_key(total.get("UnblendedCost"), "Total.UnblendedCost")?;
    require_key(metric.amount(), "Total.UnblendedCost.Amount")
}

/// Pull a group's `(Keys[0], Metrics.UnblendedCost.Amount)` pair — errors
/// loudly on any missing key (python KeyError/IndexError parity,
/// hostile-review r1 F4).
pub fn group_service_amount(g: &aws_sdk_costexplorer::types::Group) -> Result<(&str, &str), Error> {
    let svc = require_key(g.keys().first(), "Groups[].Keys[0]")?;
    let metrics = require_key(g.metrics(), "Groups[].Metrics")?;
    let metric = require_key(
        metrics.get("UnblendedCost"),
        "Groups[].Metrics.UnblendedCost",
    )?;
    let amount = require_key(metric.amount(), "Groups[].Metrics.UnblendedCost.Amount")?;
    Ok((svc.as_str(), amount))
}

/// The success return — Python parity: `{'ok': True, 'mtd_usd': .., 'pct': ..}`.
pub fn success_result(mtd_usd: f64, pct: f64) -> Value {
    json!({"ok": true, "mtd_usd": mtd_usd, "pct": pct})
}

/// Entry point — EventBridge cron invoke (payload unused, like the Python).
///
/// UNPROVEN until deploy: the live Cost Explorer + SNS legs run only in a
/// real Lambda invoke.
pub async fn handle(_event: Value) -> Result<Value, Error> {
    let topic_arn = std::env::var("ALERTS_TOPIC_ARN")
        .map_err(|_| Error::from("ALERTS_TOPIC_ARN env var is missing"))?;

    let today_utc = Utc::now().date_naive();
    let yest_utc = today_utc - Duration::days(1);
    let mtd_start = today_utc.with_day(1).unwrap_or(today_utc);

    let ce = crate::clients::cost_explorer_us_east_1().await;
    // Cost Explorer "end" is exclusive — yesterday's full day = today as end.
    let yday_usd = get_total(&ce, &yest_utc.to_string(), &today_utc.to_string()).await?;
    let mtd_usd = get_total(&ce, &mtd_start.to_string(), &today_utc.to_string()).await?;
    let by_svc_raw = get_by_service(&ce, &mtd_start.to_string(), &today_utc.to_string()).await?;
    let by_svc = aggregate_by_service(&by_svc_raw);

    let (message, pct) = render_digest(today_utc, yday_usd, mtd_usd, &by_svc);

    let config = crate::clients::sdk_config().await;
    let sns = crate::clients::sns(&config);
    sns.publish()
        .topic_arn(&topic_arn)
        .subject(DIGEST_SUBJECT)
        .message(&message)
        .send()
        .await?;
    info!(mtd_usd, pct, "daily budget digest published");

    Ok(success_result(mtd_usd, pct))
}

fn date_interval(
    start: &str,
    end: &str,
) -> Result<aws_sdk_costexplorer::types::DateInterval, Error> {
    aws_sdk_costexplorer::types::DateInterval::builder()
        .start(start)
        .end(end)
        .build()
        .map_err(Error::from)
}

/// Python `get_total` — DAILY UnblendedCost summed over the window.
async fn get_total(
    ce: &aws_sdk_costexplorer::Client,
    start: &str,
    end: &str,
) -> Result<f64, Error> {
    let r = ce
        .get_cost_and_usage()
        .time_period(date_interval(start, end)?)
        .granularity(aws_sdk_costexplorer::types::Granularity::Daily)
        .metrics("UnblendedCost")
        .send()
        .await?;
    // Hostile-review r1 F4: missing keys ERROR loudly (python KeyError
    // parity), never a silent skip into a false-OK ₹0 digest.
    let mut amounts: Vec<&str> = Vec::with_capacity(r.results_by_time().len());
    for day in r.results_by_time() {
        amounts.push(day_unblended_amount(day)?);
    }
    sum_amounts(amounts)
}

/// Python `get_by_service` — raw `(SERVICE, usd)` pairs per day-group
/// (aggregation/labeling happens in `aggregate_by_service`).
async fn get_by_service(
    ce: &aws_sdk_costexplorer::Client,
    start: &str,
    end: &str,
) -> Result<Vec<(String, f64)>, Error> {
    let r = ce
        .get_cost_and_usage()
        .time_period(date_interval(start, end)?)
        .granularity(aws_sdk_costexplorer::types::Granularity::Daily)
        .metrics("UnblendedCost")
        .group_by(
            aws_sdk_costexplorer::types::GroupDefinition::builder()
                .r#type(aws_sdk_costexplorer::types::GroupDefinitionType::Dimension)
                .key("SERVICE")
                .build(),
        )
        .send()
        .await?;
    let mut out = Vec::new();
    for day in r.results_by_time() {
        for g in day.groups() {
            // Hostile-review r1 F4: missing Keys/Metrics keys ERROR loudly
            // (python KeyError/IndexError parity), never a silent skip.
            let (svc, amount) = group_service_amount(g)?;
            let usd = amount
                .parse::<f64>()
                .map_err(|e| Error::from(format!("bad Cost Explorer amount {amount:?}: {e}")))?;
            out.push((svc.to_string(), usd));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_label_for_substring_matches() {
        assert_eq!(
            label_for("Amazon Elastic Compute Cloud - Compute"),
            "EC2 compute"
        );
        assert_eq!(label_for("EC2 - Other"), "EBS + transfer");
        assert_eq!(label_for("Amazon Virtual Private Cloud"), "Public IP / VPC");
        assert_eq!(label_for("AmazonCloudWatch"), "CloudWatch");
        assert_eq!(label_for("Amazon Simple Storage Service"), "S3 storage");
        assert_eq!(
            label_for("Amazon Simple Notification Service"),
            "SNS alerts"
        );
        assert_eq!(label_for("AWS Lambda"), "Lambda");
        assert_eq!(label_for("AWS Key Management Service"), "KMS");
        // No match → the raw service name passes through.
        assert_eq!(label_for("Amazon Route 53"), "Amazon Route 53");
    }

    #[test]
    fn test_inr_applies_rate_and_gst() {
        assert!((inr(1.0) - 100.3).abs() < 1e-9); // 85 * 1.18
        assert_eq!(inr(0.0), 0.0);
    }

    #[test]
    fn test_emoji_thresholds_match_python() {
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
    fn test_month_math_december_python_special_case() {
        // Python: `... if today_utc.month < 12 else 31`.
        let dec = NaiveDate::from_ymd_opt(2026, 12, 5).unwrap();
        let m = month_math(dec, 5.0);
        assert_eq!(m.days_in_month, 31);
        assert_eq!(m.days_remaining, 26);
    }

    #[test]
    fn test_aggregate_by_service_skips_zero_and_merges_labels() {
        let raw = vec![
            ("Amazon Elastic Compute Cloud - Compute".to_string(), 1.5),
            ("Amazon Elastic Compute Cloud - Compute".to_string(), 0.5),
            ("AWS Lambda".to_string(), 0.0),   // skipped (<= 0)
            ("AWS Lambda".to_string(), -0.25), // skipped (<= 0)
            ("Tax".to_string(), 0.1),
        ];
        let agg = aggregate_by_service(&raw);
        assert_eq!(agg.len(), 2);
        assert_eq!(agg[0], ("EC2 compute".to_string(), 2.0));
        assert_eq!(agg[1], ("Tax".to_string(), 0.1));
    }

    #[test]
    fn test_render_digest_full_message_shape() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let by_svc = vec![
            ("EC2 compute".to_string(), 4.0),
            ("CloudWatch".to_string(), 6.0),
        ];
        let (msg, pct) = render_digest(today, 0.53, 10.0, &by_svc);
        // pct = 10/55*100 ≈ 18.18 → 🟢
        assert!((pct - 18.181818).abs() < 1e-3);
        assert!(msg.starts_with("🟢 *AWS Cost — tickvault*"));
        // inr(0.53) = 53.159 → "₹53"; USD keeps 2dp with a SINGLE dollar sign.
        assert!(msg.contains("_Yesterday_:   ₹53   ($0.53)"), "{msg}");
        assert!(msg.contains("_This month_:  ₹1003   ($10.00)"), "{msg}");
        assert!(msg.contains("_Of $55 stop-budget_: 18%"), "{msg}");
        assert!(msg.contains("_Days left_:   13"), "{msg}");
        assert!(msg.contains("*Where it goes (this month):*"));
        // Sorted descending by USD: CloudWatch (6.0) before EC2 (4.0),
        // with the Python {:<16} left-justified label column.
        let cw_idx = msg.find("  CloudWatch       ₹").unwrap();
        let ec2_idx = msg.find("  EC2 compute      ₹").unwrap();
        assert!(cw_idx < ec2_idx, "{msg}");
        assert!(!msg.contains("(no spend yet this month)"));
    }

    #[test]
    fn test_render_digest_empty_month() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let (msg, pct) = render_digest(today, 0.0, 0.0, &[]);
        assert_eq!(pct, 0.0);
        assert!(msg.contains("  (no spend yet this month)"));
        assert!(msg.contains("_Of $55 stop-budget_: 0%"));
    }

    #[test]
    fn test_render_digest_over_budget_is_red() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let (msg, pct) = render_digest(today, 1.0, 60.0, &[]);
        assert!(pct > 100.0);
        assert!(msg.starts_with("🔴"));
    }

    #[test]
    fn test_service_line_format_matches_python_width() {
        // Python: f"  {nice:<16} ₹{inr(usd):.0f}  ($${usd:.2f})" — with the
        // terraform `$$` unescaping to a single `$`.
        let today = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        let by_svc = vec![("KMS".to_string(), 0.51)];
        let (msg, _) = render_digest(today, 0.0, 0.51, &by_svc);
        assert!(msg.contains("  KMS              ₹51  ($0.51)"), "{msg}");
    }

    #[test]
    fn test_sum_amounts_parses_and_sums() {
        assert!((sum_amounts(["1.5", "2.25", "0"]).unwrap() - 3.75).abs() < 1e-9);
        assert_eq!(sum_amounts([]).unwrap(), 0.0);
    }

    #[test]
    fn test_sum_amounts_rejects_garbage_like_python_float() {
        assert!(sum_amounts(["1.5", "not-a-number"]).is_err());
    }

    #[test]
    fn test_day_unblended_amount_missing_keys_error_loudly() {
        use aws_sdk_costexplorer::types::{MetricValue, ResultByTime};
        // Hostile-review r1 F4: a day with NO Total must ERROR (python
        // KeyError parity), never silently skip into a false-OK ₹0.
        let no_total = ResultByTime::builder().build();
        assert!(day_unblended_amount(&no_total).is_err());
        // Total present but the UnblendedCost metric missing also errors.
        let wrong_metric = ResultByTime::builder()
            .total("SomethingElse", MetricValue::builder().amount("1").build())
            .build();
        assert!(day_unblended_amount(&wrong_metric).is_err());
        // Metric present but Amount missing also errors.
        let no_amount = ResultByTime::builder()
            .total("UnblendedCost", MetricValue::builder().build())
            .build();
        assert!(day_unblended_amount(&no_amount).is_err());
    }

    #[test]
    fn test_day_unblended_amount_normal_shape_unchanged() {
        use aws_sdk_costexplorer::types::{MetricValue, ResultByTime};
        let day = ResultByTime::builder()
            .total(
                "UnblendedCost",
                MetricValue::builder().amount("1.25").build(),
            )
            .build();
        assert_eq!(day_unblended_amount(&day).unwrap(), "1.25");
    }

    #[test]
    fn test_group_service_amount_missing_keys_error_loudly() {
        use aws_sdk_costexplorer::types::{Group, MetricValue};
        // No Keys → error (python g['Keys'][0] IndexError parity).
        let no_keys = Group::builder()
            .metrics("UnblendedCost", MetricValue::builder().amount("1").build())
            .build();
        assert!(group_service_amount(&no_keys).is_err());
        // No Metrics → error (python g['Metrics'] KeyError parity).
        let no_metrics = Group::builder().keys("AWS Lambda").build();
        assert!(group_service_amount(&no_metrics).is_err());
        // Metrics present but UnblendedCost missing → error.
        let wrong_metric = Group::builder()
            .keys("AWS Lambda")
            .metrics("SomethingElse", MetricValue::builder().amount("1").build())
            .build();
        assert!(group_service_amount(&wrong_metric).is_err());
    }

    #[test]
    fn test_group_service_amount_normal_shape_unchanged() {
        use aws_sdk_costexplorer::types::{Group, MetricValue};
        let g = Group::builder()
            .keys("AWS Lambda")
            .metrics(
                "UnblendedCost",
                MetricValue::builder().amount("0.5").build(),
            )
            .build();
        assert_eq!(group_service_amount(&g).unwrap(), ("AWS Lambda", "0.5"));
    }

    #[test]
    fn test_require_key_some_passes_none_errors() {
        assert_eq!(require_key(Some(7_u8), "K").unwrap(), 7);
        let err = require_key(None::<u8>, "Total").unwrap_err();
        assert!(err.to_string().contains("Total"), "{err}");
    }

    #[test]
    fn test_success_result_shape() {
        let v = success_result(12.5, 22.7);
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["mtd_usd"], serde_json::json!(12.5));
        assert_eq!(v["pct"], serde_json::json!(22.7));
    }

    #[test]
    fn test_digest_subject_is_python_literal() {
        assert_eq!(DIGEST_SUBJECT, "[BUDGET] daily AWS cost");
    }
}
