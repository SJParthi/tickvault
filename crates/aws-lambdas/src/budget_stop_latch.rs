//! Budget-stop latch — one SSM parameter that keeps a budget stop stopped
//! for the rest of the billing month (audit PR30, 2026-09-27).
//!
//! Before this, a budget stop was undone the same day by three things that
//! start the box without looking at the budget: the 08:45 start watchdog's
//! self-start, `scripts/aws-autopilot.sh`'s up-window self-start, and the
//! terraform apply that set the `daily-start` rule back to ENABLED after
//! the hourly guard had disabled it. Each restart then ran until the hourly
//! guard stopped it again, paging both ways, every day for the rest of the
//! month.
//!
//! **The value is the UTC billing month (`YYYY-MM`) the stop happened in.**
//! Writers: the hourly hard-stop guard's breach stop, and the AWS-Budgets
//! kill-switch. Readers: the start watchdog, the autopilot script and the
//! terraform apply workflow. A latch counts only while its month is the
//! current UTC month, so it clears itself when the next billing month starts
//! (month-to-date spend restarts from zero then, which is the only thing that
//! can un-breach a monthly ceiling). The operator clears it early by deleting
//! the parameter.
//!
//! **UTC, not IST, for the same reason as
//! `hard_stop_guard::effective_budget_kill_usd`:** AWS bills and reports
//! month-to-date in UTC, so the latch and the spend it stands for must name
//! the same month.
//!
//! **Readers fail OPEN** (an unreadable latch = not latched): a real trading
//! day must never lose its start to an SSM outage. The hourly guard still
//! reads Cost Explorer directly and stops a breached box within the hour, so
//! a failed read costs at most one hour of box time, never the budget.

use chrono::{DateTime, Utc};

/// Default parameter name (prod). Terraform injects `BUDGET_STOP_PARAM`
/// per environment; the autopilot script and the apply workflow build the
/// same `/tickvault-guard/<env>/budget-stop-month` path.
///
/// **Deliberately OUTSIDE `/tickvault/<env>/`.** The trading box's own role
/// may `ssm:PutParameter` anywhere under `/tickvault/<env>/*` (instance lock,
/// token publish). A latch under that prefix could be written by anything
/// holding the box's credentials, and one write would keep every restart
/// path off for the rest of the month with nothing to tell it from a real
/// budget stop. Under `/tickvault-guard/` only the two writer Lambdas hold a
/// write grant, each scoped to this one ARN.
pub const DEFAULT_BUDGET_STOP_PARAM: &str = "/tickvault-guard/prod/budget-stop-month";

/// How far back the AWS-Budgets kill-switch dates its latch. AWS can deliver
/// a month's 100% notification a few hours late; one that lands just after
/// 00:00 UTC on the 1st is about the month that just ended, and a latch
/// naming the NEW month would keep the box off for a whole month it has not
/// overspent. No real breach can happen in a month's first six hours (the
/// spend has just restarted from zero), so dating six hours back is always
/// safe.
pub const KILLSWITCH_NOTIFICATION_LAG_HOURS: i64 = 6;

/// The UTC billing month of `now_utc`, as the latch stores it (`YYYY-MM`).
pub fn billing_month_utc(now_utc: DateTime<Utc>) -> String {
    now_utc.format("%Y-%m").to_string()
}

/// The month the kill-switch latches: `KILLSWITCH_NOTIFICATION_LAG_HOURS`
/// before `now_utc` (see that constant).
pub fn killswitch_billing_month(now_utc: DateTime<Utc>) -> String {
    billing_month_utc(now_utc - chrono::Duration::hours(KILLSWITCH_NOTIFICATION_LAG_HOURS))
}

/// `true` when the stored value is a well-formed latch (`YYYY-MM`) for a
/// month BEFORE the current UTC month: the stop it recorded has expired, and
/// the `daily-start` rule the guard disabled can be turned back on. Anything
/// else — the current month, a `released-…` marker, garbage, `None` — is
/// `false`, so a release happens at most once per latch.
pub fn latch_names_an_earlier_month(raw_param: Option<&str>, now_utc: DateTime<Utc>) -> bool {
    let Some(raw) = raw_param.map(str::trim) else {
        return false;
    };
    let bytes = raw.as_bytes();
    let well_formed = bytes.len() == 7
        && bytes[4] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || b.is_ascii_digit());
    // Fixed-width `YYYY-MM` compares correctly as a string.
    well_formed && raw < billing_month_utc(now_utc).as_str()
}

/// `true` when the stored latch names the CURRENT UTC billing month.
/// `None` is the SSM-error / missing-parameter arm and fails open. A latch
/// from an earlier month never matches.
pub fn budget_stop_is_latched(raw_param: Option<&str>, now_utc: DateTime<Utc>) -> bool {
    raw_param.is_some_and(|raw| raw.trim() == billing_month_utc(now_utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    #[test]
    fn test_budget_stop_latch_billing_month_utc_is_the_utc_month() {
        assert_eq!(billing_month_utc(utc(2026, 9, 27, 12, 0)), "2026-09");
        // 1 October 00:30 IST is still 30 September in UTC: the latch must
        // name September, the month the spend belongs to.
        assert_eq!(billing_month_utc(utc(2026, 9, 30, 19, 0)), "2026-09");
        assert_eq!(billing_month_utc(utc(2026, 10, 1, 0, 0)), "2026-10");
    }

    #[test]
    fn test_budget_stop_is_latched_only_for_the_current_month() {
        let now = utc(2026, 9, 27, 3, 15);
        assert!(budget_stop_is_latched(Some("2026-09"), now));
        assert!(budget_stop_is_latched(Some(" 2026-09\n"), now));
        // Last month's latch has expired on its own.
        assert!(!budget_stop_is_latched(Some("2026-08"), now));
        // A September latch no longer holds once October starts in UTC.
        assert!(!budget_stop_is_latched(
            Some("2026-09"),
            utc(2026, 10, 1, 0, 0)
        ));
        assert!(!budget_stop_is_latched(Some(""), now));
        assert!(!budget_stop_is_latched(Some("garbage"), now));
    }

    #[test]
    fn test_killswitch_billing_month_dates_a_late_notification_to_the_old_month() {
        // A September notification delivered at 02:00 UTC on 1 October
        // latches September, never October.
        assert_eq!(killswitch_billing_month(utc(2026, 10, 1, 2, 0)), "2026-09");
        assert_eq!(killswitch_billing_month(utc(2026, 10, 1, 6, 0)), "2026-10");
        assert_eq!(killswitch_billing_month(utc(2026, 9, 27, 12, 0)), "2026-09");
    }

    #[test]
    fn test_latch_names_an_earlier_month_only_for_an_expired_well_formed_latch() {
        let now = utc(2026, 10, 1, 0, 30);
        assert!(latch_names_an_earlier_month(Some("2026-09"), now));
        assert!(latch_names_an_earlier_month(Some(" 2025-12\n"), now));
        assert!(!latch_names_an_earlier_month(Some("2026-10"), now));
        // The marker written after a release never releases again.
        assert!(!latch_names_an_earlier_month(Some("released-2026-09"), now));
        assert!(!latch_names_an_earlier_month(Some("2026-9"), now));
        assert!(!latch_names_an_earlier_month(Some("20a6-09"), now));
        assert!(!latch_names_an_earlier_month(Some(""), now));
        assert!(!latch_names_an_earlier_month(None, now));
    }

    #[test]
    fn test_budget_stop_is_latched_fails_open_on_an_unreadable_parameter() {
        assert!(!budget_stop_is_latched(None, utc(2026, 9, 27, 3, 15)));
    }
}
