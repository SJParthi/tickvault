//! Coverage for `holiday_gate::run` on every verdict that lets the app start.
//!
//! The stop arm (`CalendarDay::WeekdayHoliday` inside 08:25-09:00 IST) calls
//! the real instance-metadata service and AWS, so it is never driven here:
//! only `TradingDay`, `Weekend`, `SpecialSession` and `Unreadable`, each of
//! which starts the app whatever the clock reads.

mod cov_xvs_support;

use std::cell::Cell;
use std::path::Path;

use tickvault_app::holiday_gate::{CalendarDay, run};

const OVERRIDE_MARKER: &str = "/opt/tickvault/ALLOW_HOLIDAY_RUN";

async fn run_with(day: CalendarDay) -> (i32, Option<chrono::NaiveDate>) {
    let asked = Cell::new(None);
    let code = run(|d| {
        asked.set(Some(d));
        day
    })
    .await;
    (code, asked.get())
}

fn expected_asked(today: chrono::NaiveDate) -> Option<chrono::NaiveDate> {
    // The override marker wins before the calendar is asked.
    (!Path::new(OVERRIDE_MARKER).exists()).then_some(today)
}

#[tokio::test]
async fn test_holiday_gate_run_trading_day_lets_the_app_start_and_asks_today() {
    cov_xvs_support::init_tracing();
    let (before, _) = cov_xvs_support::ist_now();
    let (code, asked) = run_with(CalendarDay::TradingDay).await;
    let (after, _) = cov_xvs_support::ist_now();
    assert_eq!(code, 0, "a trading day must let the app start");
    if before == after {
        assert_eq!(asked, expected_asked(before));
    }
}

#[tokio::test]
async fn test_holiday_gate_run_weekend_lets_the_app_start() {
    cov_xvs_support::init_tracing();
    let (code, asked) = run_with(CalendarDay::Weekend).await;
    assert_eq!(code, 0, "a weekend boot is a deploy or manual start");
    if !Path::new(OVERRIDE_MARKER).exists() {
        assert!(asked.is_some(), "the calendar is asked about today");
    }
}

#[tokio::test]
async fn test_holiday_gate_run_special_session_lets_the_app_start() {
    cov_xvs_support::init_tracing();
    let (code, _) = run_with(CalendarDay::SpecialSession).await;
    assert_eq!(code, 0, "a Muhurat or mock session must start the app");
}

#[tokio::test]
async fn test_holiday_gate_run_unreadable_calendar_fails_open() {
    cov_xvs_support::init_tracing();
    let (code, _) = run_with(CalendarDay::Unreadable).await;
    assert_eq!(code, 0, "an unreadable calendar must fail open");
}

#[test]
fn test_holiday_gate_is_invocation_matches_only_the_subcommand_argument() {
    use tickvault_app::holiday_gate::{HOLIDAY_GATE_SUBCOMMAND, is_invocation};
    let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert!(is_invocation(&args(&[
        "tickvault",
        HOLIDAY_GATE_SUBCOMMAND
    ])));
    assert!(!is_invocation(&args(&["tickvault"])));
    assert!(!is_invocation(&args(&[])));
    assert!(!is_invocation(&args(&["tickvault", "holiday-gate-x"])));
    assert!(!is_invocation(&args(&[HOLIDAY_GATE_SUBCOMMAND])));
}
