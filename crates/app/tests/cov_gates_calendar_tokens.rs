//! Coverage for four small app modules whose production lines no other
//! integration test reaches: `holiday_gate` (the pure calendar verdict across
//! every date and flag), `calendar_staleness` (the page decision, the
//! display helper and the watchdog's first iterations), `drain_hold` (the pure
//! helpers `cov_drain_hold.rs` leaves out: the hold window, the expiry, the
//! parameter value, the floor-blocked and evening-replay decisions) and
//! `api_token_rotation` (the supervised reload task up to its first wait).
//!
//! Every test is deterministic: explicit dates and times, no AWS call, no
//! network, no file written. The two spawned watchdog tests stop before any
//! path that would talk to SSM or write a state file.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use chrono::{Datelike, NaiveDate};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use proptest::prelude::*;
use tickvault_app::calendar_staleness::{
    CALENDAR_STALENESS_WEEK_PATH, WEEKLY_PAGE_ABOVE_DAYS, coverage_end_display, should_page_now,
    spawn_calendar_staleness_watchdog,
};
use tickvault_app::drain_hold::{
    DRAIN_HOLD_CEILING_SECS_IST, DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS, DRAIN_HOLD_IDLE_LAG,
    DRAIN_HOLD_STALL_SECS, DRAIN_HOLD_START_SECS_IST, DRAIN_HOLD_TTL_SECS, DayWork,
    ProgressTracker, drain_hold_param, drain_progress_count, evening_replay_pending,
    evening_replay_value, floor_blocked_secs, hold_until_unix, hold_value, in_hold_window,
    replay_pending_from, stall_releases_hold, stop_follows_tonights_hold,
};
use tickvault_app::evening_drain::EveningStep;
use tickvault_app::holiday_gate::{CalendarDay, classify_calendar_day};
use tickvault_common::config::{NseHolidayEntry, TradingConfig};
use tickvault_common::constants::CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};
use tickvault_core::notification::NotificationService;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid date") // APPROVED: test
}

/// The process recorder for this test binary, installed once.
fn recorder() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("install the test recorder") // APPROVED: test
    })
}

/// The value of an unlabelled series line `name value`, if rendered.
fn rendered_value(render: &str, name: &str) -> Option<f64> {
    render.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(' ')?;
        rest.trim().parse::<f64>().ok()
    })
}

fn trading_config(holidays: &[&str]) -> TradingConfig {
    TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "16:00:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: holidays
            .iter()
            .map(|d| NseHolidayEntry {
                date: (*d).to_string(),
                name: "Test holiday".to_string(),
            })
            .collect(),
        muhurat_trading_dates: Vec::new(),
        nse_mock_trading_dates: Vec::new(),
    }
}

fn ist_today() -> NaiveDate {
    chrono::Utc::now().with_timezone(&ist_offset()).date_naive()
}

// ---------------------------------------------------------------------------
// holiday_gate
// ---------------------------------------------------------------------------

#[test]
fn test_holiday_gate_classify_calendar_day_table() {
    // (date, weekday holiday flag, special-session flag, expected)
    let cases = [
        // Monday to Friday, no flags: trading days.
        (date(2026, 10, 19), false, false, CalendarDay::TradingDay),
        (date(2026, 10, 23), false, false, CalendarDay::TradingDay),
        // A weekday in the holiday list.
        (date(2026, 10, 20), true, false, CalendarDay::WeekdayHoliday),
        (date(2026, 12, 25), true, false, CalendarDay::WeekdayHoliday),
        // Saturday and Sunday, listed or not.
        (date(2026, 10, 24), false, false, CalendarDay::Weekend),
        (date(2026, 10, 25), false, false, CalendarDay::Weekend),
        (date(2026, 10, 24), true, false, CalendarDay::Weekend),
        (date(2026, 10, 25), true, false, CalendarDay::Weekend),
        // A Muhurat or mock session wins over a weekend and a holiday.
        (date(2026, 11, 8), false, true, CalendarDay::SpecialSession),
        (date(2026, 11, 8), true, true, CalendarDay::SpecialSession),
        (date(2026, 10, 20), true, true, CalendarDay::SpecialSession),
        (date(2026, 10, 21), false, true, CalendarDay::SpecialSession),
        // Extreme representable dates do not panic.
        (NaiveDate::MIN, false, true, CalendarDay::SpecialSession),
        (NaiveDate::MAX, false, true, CalendarDay::SpecialSession),
    ];
    for (day, holiday, special, expected) in cases {
        assert_eq!(
            classify_calendar_day(day, holiday, special),
            expected,
            "{day} holiday={holiday} special={special}"
        );
    }
}

#[test]
fn test_holiday_gate_classify_extreme_dates_follow_their_weekday() {
    for day in [NaiveDate::MIN, NaiveDate::MAX, date(1970, 1, 1)] {
        let weekend = matches!(day.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
        let expected_plain = if weekend {
            CalendarDay::Weekend
        } else {
            CalendarDay::TradingDay
        };
        let expected_holiday = if weekend {
            CalendarDay::Weekend
        } else {
            CalendarDay::WeekdayHoliday
        };
        assert_eq!(classify_calendar_day(day, false, false), expected_plain);
        assert_eq!(classify_calendar_day(day, true, false), expected_holiday);
    }
}

proptest! {
    #[test]
    fn test_holiday_gate_classify_never_stops_on_a_weekend_or_special_session(
        days_from_epoch in -100_000_i64..100_000,
        holiday in any::<bool>(),
        special in any::<bool>(),
    ) {
        let day = date(1970, 1, 1) + chrono::TimeDelta::days(days_from_epoch);
        let verdict = classify_calendar_day(day, holiday, special);
        let weekend = matches!(day.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
        // The only verdict the gate stops on needs a weekday holiday with no session.
        prop_assert_eq!(
            verdict == CalendarDay::WeekdayHoliday,
            !special && !weekend && holiday
        );
        prop_assert_eq!(verdict == CalendarDay::SpecialSession, special);
        prop_assert_eq!(verdict == CalendarDay::Weekend, !special && weekend);
        prop_assert_eq!(
            verdict == CalendarDay::TradingDay,
            !special && !weekend && !holiday
        );
        prop_assert_ne!(verdict, CalendarDay::Unreadable);
    }
}

// ---------------------------------------------------------------------------
// calendar_staleness
// ---------------------------------------------------------------------------

fn week_of(day: NaiveDate) -> (i32, u32) {
    let w = day.iso_week();
    (w.year(), w.week())
}

#[test]
fn test_calendar_staleness_coverage_end_display_renders_day_month_year_or_none() {
    assert_eq!(
        coverage_end_display(Some(date(2026, 12, 31))),
        "31 Dec 2026"
    );
    assert_eq!(coverage_end_display(Some(date(2027, 1, 5))), "05 Jan 2027");
    assert_eq!(coverage_end_display(Some(date(1970, 1, 1))), "01 Jan 1970");
    assert_eq!(coverage_end_display(None), "none configured");
}

#[test]
fn test_calendar_staleness_should_page_now_threshold_and_bands() {
    let today = date(2026, 11, 25); // a Wednesday
    let this_week = Some(week_of(today));
    let last_week = Some(week_of(date(2026, 11, 18)));
    let t = CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS;
    assert_eq!(t, 60);
    assert_eq!(WEEKLY_PAGE_ABOVE_DAYS, 14);
    // (days left, last paged week, expected)
    let cases = [
        // Not stale: exactly the threshold or more never pages.
        (Some(60), None, false),
        (Some(61), None, false),
        (Some(i64::MAX), None, false),
        // Weekly band (15..=59): pages only when this week was not paged.
        (Some(59), None, true),
        (Some(59), last_week, true),
        (Some(59), this_week, false),
        (Some(15), this_week, false),
        (Some(15), last_week, true),
        // Every-boot band (14 or fewer, run out): the week does not matter.
        (Some(14), this_week, true),
        (Some(1), this_week, true),
        (Some(0), this_week, true),
        (Some(-5), this_week, true),
        (Some(i64::MIN), this_week, true),
        // Empty calendar is maximally stale and in the every-boot band.
        (None, this_week, true),
        (None, None, true),
    ];
    for (days, week, expected) in cases {
        assert_eq!(
            should_page_now(days, t, today, None, week),
            expected,
            "days={days:?} week={week:?}"
        );
    }
}

#[test]
fn test_calendar_staleness_should_page_now_never_twice_on_one_ist_day() {
    let today = date(2026, 12, 20);
    for days in [None, Some(-3), Some(0), Some(14), Some(30), Some(59)] {
        assert!(!should_page_now(days, 60, today, Some(today), None));
        // Yesterday's page does not block today's.
        let yesterday = today.pred_opt().expect("valid"); // APPROVED: test
        assert!(should_page_now(days, 60, today, Some(yesterday), None));
        // In the weekly band, a page earlier this week still blocks it.
        let blocked_by_week = days.is_some_and(|d| d > WEEKLY_PAGE_ABOVE_DAYS);
        assert_eq!(
            should_page_now(days, 60, today, Some(yesterday), Some(week_of(today))),
            !blocked_by_week
        );
    }
}

#[test]
fn test_calendar_staleness_should_page_now_iso_week_crosses_the_year_boundary() {
    // 2027-01-01 (Friday) is in ISO week 53 of 2026; 2027-01-04 (Monday) opens week 1 of 2027.
    assert_eq!(week_of(date(2027, 1, 1)), (2026, 53));
    assert_eq!(week_of(date(2027, 1, 4)), (2027, 1));
    let paged = Some((2026, 53));
    assert!(!should_page_now(
        Some(40),
        60,
        date(2027, 1, 1),
        None,
        paged
    ));
    assert!(!should_page_now(
        Some(40),
        60,
        date(2027, 1, 3),
        None,
        paged
    ));
    assert!(should_page_now(Some(40), 60, date(2027, 1, 4), None, paged));
    // A week number paged in another year never matches this one.
    assert!(should_page_now(
        Some(40),
        60,
        date(2027, 1, 1),
        None,
        Some((2027, 53))
    ));
}

#[test]
fn test_calendar_staleness_should_page_now_custom_thresholds() {
    let today = date(2026, 6, 1);
    // Threshold 0: only a run-out calendar is stale.
    assert!(!should_page_now(Some(0), 0, today, None, None));
    assert!(should_page_now(Some(-1), 0, today, None, None));
    // Threshold 1: exactly zero days left is stale (strict <).
    assert!(should_page_now(Some(0), 1, today, None, None));
    // A huge threshold puts a far horizon in the weekly band.
    assert!(should_page_now(Some(10_000), i64::MAX, today, None, None));
    assert!(!should_page_now(
        Some(10_000),
        i64::MAX,
        today,
        None,
        Some(week_of(today))
    ));
    // A negative threshold: stale only below it (or an empty calendar).
    assert!(!should_page_now(Some(-50), -50, today, None, None));
    assert!(!should_page_now(Some(-40), -50, today, None, None));
    assert!(should_page_now(Some(-51), -50, today, None, None));
    assert!(should_page_now(None, -50, today, None, None));
}

proptest! {
    #[test]
    fn test_calendar_staleness_should_page_now_invariants(
        days in proptest::option::of(-1_000_i64..1_000),
        threshold in -100_i64..400,
        offset in 0_i64..20_000,
        alerted_today in any::<bool>(),
        paged_this_week in any::<bool>(),
    ) {
        let today = date(2000, 1, 1) + chrono::TimeDelta::days(offset);
        let last_alerted = alerted_today.then_some(today);
        let week = paged_this_week.then(|| week_of(today));
        let page = should_page_now(days, threshold, today, last_alerted, week);
        let stale = days.is_none_or(|d| d < threshold);
        if alerted_today || !stale {
            prop_assert!(!page);
        } else if days.is_some_and(|d| d > WEEKLY_PAGE_ABOVE_DAYS) {
            prop_assert_eq!(page, !paged_this_week);
        } else {
            prop_assert!(page);
        }
    }
}

/// The watchdog with a calendar covering far ahead sets the coverage gauge to
/// the exact days left and pages nothing (no counter, no state file written).
#[tokio::test]
async fn test_calendar_staleness_watchdog_sets_gauge_and_never_pages_far_horizon() {
    let handle = recorder();
    let week_file = std::path::Path::new(CALENDAR_STALENESS_WEEK_PATH);
    let existed_before = week_file.exists();
    let calendar = Arc::new(
        TradingCalendar::from_config(&trading_config(&["2099-01-01"])).expect("valid calendar"), // APPROVED: test
    );
    assert_eq!(calendar.coverage_end_date(), Some(date(2099, 12, 31)));
    let before = ist_today();
    let task = spawn_calendar_staleness_watchdog(
        Arc::clone(&calendar),
        Arc::new(ArcSwapOption::<NotificationService>::empty()),
    );
    let mut value = None;
    for _ in 0..500 {
        value = rendered_value(&handle.render(), "tv_calendar_coverage_days_remaining");
        if value.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let after = ist_today();
    let value = value.expect("the watchdog set the coverage gauge"); // APPROVED: test
    let expected_before = calendar.coverage_days_remaining(before).expect("covered"); // APPROVED: test
    let expected_after = calendar.coverage_days_remaining(after).expect("covered"); // APPROVED: test
    #[allow(clippy::cast_precision_loss)] // APPROVED: small day counts
    let matches = value == expected_before as f64 || value == expected_after as f64;
    assert!(matches, "{value} vs {expected_before}/{expected_after}");
    assert!(
        value > f64::from(u16::MAX) / 10.0,
        "decades of coverage left"
    );
    assert!(
        !task.is_finished(),
        "the watchdog sleeps for its next check"
    );
    assert!(
        !handle
            .render()
            .contains("tv_calendar_staleness_alerts_total"),
        "far coverage never pages"
    );
    assert_eq!(week_file.exists(), existed_before, "no week file written");
    task.abort();
}

/// An empty calendar is maximally stale, but with no notifier the watchdog
/// only retries every 30 s: it never pages, never counts and never records a week.
#[tokio::test(start_paused = true)]
async fn test_calendar_staleness_watchdog_empty_calendar_without_notifier_retries_and_never_pages()
{
    let handle = recorder();
    let week_file = std::path::Path::new(CALENDAR_STALENESS_WEEK_PATH);
    let existed_before = week_file.exists();
    let calendar = Arc::new(TradingCalendar::from_config(&trading_config(&[])).expect("valid")); // APPROVED: test
    assert_eq!(calendar.coverage_end_date(), None);
    assert_eq!(
        coverage_end_display(calendar.coverage_end_date()),
        "none configured"
    );
    let slot = Arc::new(ArcSwapOption::<NotificationService>::empty());
    let task = spawn_calendar_staleness_watchdog(calendar, Arc::clone(&slot));
    for _ in 0..12 {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(31)).await;
    }
    assert!(!task.is_finished(), "the retry loop keeps running");
    assert!(
        !handle
            .render()
            .contains("tv_calendar_staleness_alerts_total"),
        "no page is counted while no notifier exists"
    );
    assert_eq!(week_file.exists(), existed_before, "no week file written");
    task.abort();
}

// ---------------------------------------------------------------------------
// drain_hold
// ---------------------------------------------------------------------------

#[test]
fn test_drain_hold_param_names_the_environment() {
    assert_eq!(drain_hold_param("prod"), "/tickvault/prod/drain-hold");
    assert_eq!(drain_hold_param("dev"), "/tickvault/dev/drain-hold");
    assert_eq!(drain_hold_param(""), "/tickvault//drain-hold");
}

#[test]
fn test_drain_hold_in_hold_window_boundaries() {
    assert_eq!(DRAIN_HOLD_START_SECS_IST, 56_400);
    assert_eq!(DRAIN_HOLD_CEILING_SECS_IST, 27_000);
    let cases = [
        (0, true),
        (DRAIN_HOLD_CEILING_SECS_IST - 1, true),
        (DRAIN_HOLD_CEILING_SECS_IST, false),
        (9 * 3600, false),
        (DRAIN_HOLD_START_SECS_IST - 1, false),
        (DRAIN_HOLD_START_SECS_IST, true),
        (86_399, true),
        // Out-of-range seconds read as inside (>= start).
        (86_400, true),
        (u64::MAX, true),
    ];
    for (secs, expected) in cases {
        assert_eq!(in_hold_window(secs), expected, "secs={secs}");
    }
}

#[test]
fn test_drain_hold_hold_until_unix_is_ttl_ahead_cut_at_0730() {
    let now = 1_800_000_000_i64;
    let cases = [
        // Evening: the 07:30 ceiling is far, so the full TTL.
        (DRAIN_HOLD_START_SECS_IST, now + DRAIN_HOLD_TTL_SECS),
        (0, now + DRAIN_HOLD_TTL_SECS),
        (7 * 3600, now + DRAIN_HOLD_TTL_SECS),
        // 20 minutes before 07:30: exactly the TTL.
        (DRAIN_HOLD_CEILING_SECS_IST - 1_200, now + 1_200),
        // Closer than the TTL: cut at 07:30.
        (DRAIN_HOLD_CEILING_SECS_IST - 300, now + 300),
        (DRAIN_HOLD_CEILING_SECS_IST - 1, now + 1),
        // At 07:30 exactly the next ceiling is a day away.
        (DRAIN_HOLD_CEILING_SECS_IST, now + DRAIN_HOLD_TTL_SECS),
    ];
    for (secs, expected) in cases {
        assert_eq!(hold_until_unix(now, secs), expected, "secs={secs}");
    }
    // A negative unix time is passed through unchanged plus the lead.
    assert_eq!(hold_until_unix(-10, 0), -10 + DRAIN_HOLD_TTL_SECS);
}

proptest! {
    #[test]
    fn test_drain_hold_hold_until_never_past_the_ttl_or_the_ceiling(
        now in -1_000_000_000_i64..4_000_000_000,
        secs in 0_u64..86_400,
    ) {
        let until = hold_until_unix(now, secs);
        let ahead = until - now;
        prop_assert!((1..=DRAIN_HOLD_TTL_SECS).contains(&ahead));
        let to_ceiling = if secs < DRAIN_HOLD_CEILING_SECS_IST {
            DRAIN_HOLD_CEILING_SECS_IST - secs
        } else {
            86_400 - secs + DRAIN_HOLD_CEILING_SECS_IST
        };
        prop_assert_eq!(ahead, i64::try_from(to_ceiling).unwrap_or(i64::MAX).min(DRAIN_HOLD_TTL_SECS));
    }

    #[test]
    fn test_drain_hold_stop_follows_tonights_hold_is_false_with_no_hold_written(secs in any::<u32>()) {
        // Nothing in this process wrote a hold, so no stop is the hold's stop.
        prop_assert!(!stop_follows_tonights_hold(secs));
    }
}

#[test]
fn test_drain_hold_hold_value_json_contract() {
    let held: serde_json::Value = serde_json::from_str(&hold_value(
        true,
        1_700_001_200,
        1_700_000_000,
        "spill_files=2",
    ))
    .expect("json"); // APPROVED: test
    assert_eq!(
        held,
        serde_json::json!({
            "v": 1,
            "state": "holding",
            "until_unix": 1_700_001_200_i64,
            "written_unix": 1_700_000_000_i64,
            "left": "spill_files=2",
        })
    );
    // A release always writes until_unix 0, whatever was passed.
    let released: serde_json::Value =
        serde_json::from_str(&hold_value(false, 1_700_001_200, 1_700_000_000, "")).expect("json"); // APPROVED: test
    assert_eq!(released["state"], "released");
    assert_eq!(released["until_unix"], 0);
    assert_eq!(released["written_unix"], 1_700_000_000_i64);
    assert_eq!(released["left"], "");
    // Quotes and backslashes in the summary are escaped, never break the JSON.
    let odd = hold_value(true, i64::MAX, i64::MIN, "a\"b\\c\n");
    let parsed: serde_json::Value = serde_json::from_str(&odd).expect("json"); // APPROVED: test
    assert_eq!(parsed["left"], "a\"b\\c\n");
    assert_eq!(parsed["until_unix"], i64::MAX);
    assert_eq!(parsed["written_unix"], i64::MIN);
}

#[test]
fn test_drain_hold_floor_blocked_secs_from_the_published_start() {
    assert_eq!(floor_blocked_secs(None, 1_000), None);
    assert_eq!(floor_blocked_secs(Some(1_000), 1_000), Some(0));
    assert_eq!(floor_blocked_secs(Some(1_000), 4_600), Some(3_600));
    // A clock behind the start reads as still blocked, 0 seconds.
    assert_eq!(floor_blocked_secs(Some(5_000), 1_000), Some(0));
    // Extremes saturate instead of overflowing.
    assert_eq!(
        floor_blocked_secs(Some(i64::MIN), i64::MAX),
        Some(u64::try_from(i64::MAX).expect("fits"))
    );
    assert_eq!(floor_blocked_secs(Some(i64::MAX), i64::MIN), Some(0));
}

#[test]
fn test_drain_hold_deferred_floor_blocked_cap_and_bucket_gauge() {
    let at = |secs: Option<u64>, marks: usize| DayWork {
        deferred_floor_blocked_secs: secs,
        deferred_marks: marks,
        ..DayWork::default()
    };
    assert_eq!(DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS, 3_600);
    assert!(!at(None, 9).deferred_floor_blocked());
    assert!(!at(Some(0), 9).deferred_floor_blocked());
    assert!(!at(Some(DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS - 1), 9).deferred_floor_blocked());
    assert!(at(Some(DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS), 9).deferred_floor_blocked());
    assert!(at(Some(u64::MAX), 9).deferred_floor_blocked());
    // The gauge shows the marks only while a pass is floor-blocked at all.
    assert_eq!(at(None, 9).floor_blocked_buckets(), 0);
    assert_eq!(at(Some(0), 9).floor_blocked_buckets(), 9);
    assert_eq!(at(Some(5), 0).floor_blocked_buckets(), 0);
    assert_eq!(
        at(Some(u64::MAX), usize::MAX).floor_blocked_buckets(),
        usize::MAX
    );
}

/// Every part known and empty: the day is in the database.
fn clean_work() -> DayWork {
    DayWork {
        seal_staged: Some(0),
        apply_lag: Some(0),
        suspended: Some(0),
        wal_backlog: Some(0),
        ..DayWork::default()
    }
}

#[test]
fn test_drain_hold_data_in_database_each_part_holds() {
    assert!(clean_work().data_in_database());
    assert!(clean_work().is_done());
    assert!(clean_work().evening_work_done());
    // Each change alone makes the day not done.
    let not_done = [
        DayWork {
            deferred_marks: 1,
            ..clean_work()
        },
        DayWork {
            deferred_overflowed: true,
            ..clean_work()
        },
        DayWork {
            spill_files: 1,
            ..clean_work()
        },
        DayWork {
            seal_staged: Some(1),
            ..clean_work()
        },
        DayWork {
            seal_staged: None,
            ..clean_work()
        },
        DayWork {
            apply_lag: Some(DRAIN_HOLD_IDLE_LAG + 1),
            ..clean_work()
        },
        DayWork {
            apply_lag: None,
            ..clean_work()
        },
        DayWork {
            suspended: Some(1),
            ..clean_work()
        },
        DayWork {
            suspended: None,
            ..clean_work()
        },
        DayWork {
            sink_suspect: true,
            ..clean_work()
        },
        DayWork {
            wal_holding: true,
            ..clean_work()
        },
    ];
    for work in not_done {
        assert!(!work.data_in_database(), "{}", work.left_text());
        assert!(!work.is_done(), "{}", work.left_text());
        assert!(!work.evening_work_done(), "{}", work.left_text());
    }
    // The idle lag itself still counts as caught up.
    assert!(
        DayWork {
            apply_lag: Some(DRAIN_HOLD_IDLE_LAG),
            ..clean_work()
        }
        .data_in_database()
    );
    // Held-unensured spill files and an unknown WAL backlog are not work.
    assert!(
        DayWork {
            spill_files_held_unensured: 7,
            ..clean_work()
        }
        .is_done()
    );
    assert!(
        DayWork {
            wal_backlog: None,
            ..clean_work()
        }
        .is_done()
    );
}

#[test]
fn test_drain_hold_deferred_marks_released_by_retryable_zero_or_floor_cap() {
    let marked = DayWork {
        deferred_marks: 40,
        deferred_overflowed: true,
        ..clean_work()
    };
    assert!(!marked.data_in_database());
    // A finished pass with nothing a rerun could finish releases the marks.
    assert!(
        DayWork {
            deferred_retryable_left: Some(0),
            ..marked.clone()
        }
        .data_in_database()
    );
    assert!(
        !DayWork {
            deferred_retryable_left: Some(1),
            ..marked.clone()
        }
        .data_in_database()
    );
    // An hour floor-blocked releases them too; a minute less does not.
    assert!(
        DayWork {
            deferred_floor_blocked_secs: Some(DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS),
            ..marked.clone()
        }
        .data_in_database()
    );
    assert!(
        !DayWork {
            deferred_floor_blocked_secs: Some(DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS - 60),
            ..marked
        }
        .data_in_database()
    );
}

#[test]
fn test_drain_hold_archive_and_export_hold_only_the_box() {
    // Archive copy work keeps the box but not the evening restart.
    let archive = DayWork {
        archive_backlog_bytes: Some(1),
        ..clean_work()
    };
    assert!(archive.data_in_database());
    assert!(!archive.is_done());
    assert!(archive.evening_work_done());
    // A due export holds the box; only a running one holds the restart.
    let pending = DayWork {
        brutex_export_pending: true,
        ..clean_work()
    };
    assert!(!pending.is_done());
    assert!(pending.evening_work_done());
    let running = DayWork {
        brutex_export_running: true,
        ..clean_work()
    };
    assert!(running.is_done());
    assert!(!running.evening_work_done());
}

#[test]
fn test_drain_hold_left_text_renders_every_part() {
    assert_eq!(
        DayWork::default().left_text(),
        "depth_buckets=0 spill_files=0 seal_files=unknown db_backlog=unknown suspended=unknown \
         wal_segments=unknown"
    );
    assert_eq!(
        clean_work().left_text(),
        "depth_buckets=0 spill_files=0 seal_files=0 db_backlog=0 suspended=0 wal_segments=0"
    );
    let busy = DayWork {
        deferred_marks: 12,
        deferred_overflowed: true,
        deferred_floor_blocked_secs: Some(10),
        spill_files: 3,
        spill_files_held_unensured: 2,
        seal_staged: Some(4),
        apply_lag: Some(900),
        suspended: Some(1),
        sink_suspect: true,
        wal_backlog: Some(55),
        wal_holding: true,
        archive_backlog_bytes: Some(4_096),
        brutex_export_running: true,
        brutex_export_pending: true,
        ..DayWork::default()
    };
    assert_eq!(
        busy.left_text(),
        "depth_buckets=12+overflow+floor_blocked spill_files=3 seal_files=4 db_backlog=900 \
         suspended=1 sink_suspect wal_segments=55+replaying archive_bytes=4096 \
         spill_files_held_for_table=2 brutex_export=running"
    );
    // The provisional archive reading and a merely pending export.
    let provisional = DayWork {
        archive_backlog_bytes: Some(tickvault_app::archive_backlog::PROVISIONAL_BACKLOG_BYTES),
        brutex_export_pending: true,
        ..clean_work()
    };
    assert_eq!(
        provisional.left_text(),
        "depth_buckets=0 spill_files=0 seal_files=0 db_backlog=0 suspended=0 wal_segments=0 \
         archive_bytes=pass_running brutex_export=pending"
    );
    // Zero archive bytes are still shown (Some(0) holds nothing but is known).
    assert!(
        DayWork {
            archive_backlog_bytes: Some(0),
            ..clean_work()
        }
        .left_text()
        .ends_with(" archive_bytes=0")
    );
}

#[test]
fn test_drain_hold_stall_releases_hold_only_without_a_due_export() {
    let idle = DayWork::default();
    let export = DayWork {
        brutex_export_pending: true,
        ..DayWork::default()
    };
    assert!(stall_releases_hold(true, &idle));
    assert!(!stall_releases_hold(false, &idle));
    assert!(!stall_releases_hold(true, &export));
    assert!(!stall_releases_hold(false, &export));
    // A running (not due) export does not stop the stall release.
    let running = DayWork {
        brutex_export_running: true,
        ..DayWork::default()
    };
    assert!(stall_releases_hold(true, &running));
}

#[test]
fn test_drain_hold_evening_replay_value_truth_table() {
    for step in [
        EveningStep::Idle,
        EveningStep::Wait,
        EveningStep::Restart,
        EveningStep::GaveUp,
    ] {
        for backlog in [None, Some(0), Some(1), Some(u64::MAX)] {
            for stalled in [false, true] {
                let expected = !stalled
                    && matches!(step, EveningStep::Wait | EveningStep::Restart)
                    && backlog.is_some_and(|n| n > 0);
                assert_eq!(
                    evening_replay_value(step, backlog, stalled),
                    expected,
                    "{step:?} {backlog:?} stalled={stalled}"
                );
            }
        }
    }
}

#[test]
fn test_drain_hold_replay_pending_from_truth_table() {
    for bits in 0_u8..16 {
        let published_value = bits & 1 != 0;
        let published = bits & 2 != 0;
        let planned = bits & 4 != 0;
        let running = bits & 8 != 0;
        let expected = running || published_value || (!published && planned);
        assert_eq!(
            replay_pending_from(published_value, published, planned, running),
            expected,
            "value={published_value} published={published} planned={planned} running={running}"
        );
    }
    // Once published false, a planned boot no longer reads pending.
    assert!(!replay_pending_from(false, true, true, false));
    assert!(replay_pending_from(false, false, true, false));
}

#[test]
fn test_drain_hold_evening_replay_pending_is_false_in_a_fresh_process() {
    // No supervisor published, this is not a planned evening boot and no
    // replay registered itself: nothing will write old rows.
    assert!(!evening_replay_pending());
}

#[test]
fn test_drain_hold_drain_progress_count_adds_and_saturates() {
    assert_eq!(drain_progress_count(0, 0), 0);
    assert_eq!(drain_progress_count(10, 5), 15);
    assert_eq!(drain_progress_count(u64::MAX, 1), u64::MAX);
    assert_eq!(drain_progress_count(1, u64::MAX), u64::MAX);
}

#[test]
fn test_drain_hold_observe_with_archive_drop_counts_only_while_archive_holds() {
    // Archive backlog not holding: a drop is not progress.
    let stuck = DayWork {
        spill_files: 2,
        ..DayWork::default()
    };
    let mut tracker = ProgressTracker::default();
    assert!(!tracker.observe_with_archive(&stuck, 0, 0, 0));
    assert!(!tracker.observe_with_archive(&stuck, 0, 5, DRAIN_HOLD_STALL_SECS - 1));
    assert!(tracker.observe_with_archive(&stuck, 0, 9, DRAIN_HOLD_STALL_SECS));

    // Archive backlog holding at a flat level: each drop moves the clock.
    let archive = DayWork {
        archive_backlog_bytes: Some(1_000),
        ..DayWork::default()
    };
    let mut tracker = ProgressTracker::default();
    assert!(!tracker.observe_with_archive(&archive, 0, 0, 0));
    assert!(!tracker.observe_with_archive(&archive, 0, 1, DRAIN_HOLD_STALL_SECS));
    // No further drop: stalled an hour after the last drop, not after the anchor.
    assert!(!tracker.observe_with_archive(&archive, 0, 1, 2 * DRAIN_HOLD_STALL_SECS - 1));
    assert!(tracker.observe_with_archive(&archive, 0, 1, 2 * DRAIN_HOLD_STALL_SECS));
}

#[test]
fn test_drain_hold_observe_with_archive_drop_does_not_reanchor_a_swinging_part() {
    let at = |lag: u64| DayWork {
        apply_lag: Some(lag),
        archive_backlog_bytes: Some(500),
        ..DayWork::default()
    };
    let mut tracker = ProgressTracker::default();
    assert!(!tracker.observe_with_archive(&at(100), 0, 0, 0));
    // The lag rises while a drop lands: progress by the drop, anchor stays 100.
    assert!(!tracker.observe_with_archive(&at(300), 0, 1, 10));
    // A dip to 200 is still above the anchor 100: not progress.
    assert!(!tracker.observe_with_archive(&at(200), 0, 1, 20));
    assert!(tracker.observe_with_archive(&at(200), 0, 1, 10 + DRAIN_HOLD_STALL_SECS));
    // Falling below the anchor is progress again.
    assert!(!tracker.observe_with_archive(&at(99), 0, 1, 20 + DRAIN_HOLD_STALL_SECS));
}

// ---------------------------------------------------------------------------
// api_token_rotation
// ---------------------------------------------------------------------------

/// The supervised reload task registers every outcome series at 0 and then
/// waits for its first 5-minute tick or a hint: no SSM read, no respawn.
#[tokio::test(start_paused = true)]
async fn test_api_token_rotation_supervisor_registers_outcomes_and_waits() {
    use tickvault_app::api_token_rotation::{
        API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD, API_TOKEN_RELOAD_INTERVAL_SECS,
        spawn_supervised_api_token_reload,
    };
    assert_eq!(API_TOKEN_RELOAD_INTERVAL_SECS, 300);
    assert_eq!(API_TOKEN_RELOAD_FAILURE_WARN_THRESHOLD, 3);
    let handle = recorder();
    let token = "Ab3$".repeat(10);
    let config = tickvault_api::middleware::ApiAuthConfig::new(token.clone());
    let task = spawn_supervised_api_token_reload(config.clone());
    let wanted = ["ok", "unchanged", "failed", "rejected"]
        .map(|outcome| format!("tv_api_token_reloads_total{{outcome=\"{outcome}\"}} 0"));
    let mut render = String::new();
    for _ in 0..500 {
        tokio::task::yield_now().await;
        render = handle.render();
        if wanted.iter().all(|line| render.lines().any(|l| l == line)) {
            break;
        }
    }
    for line in &wanted {
        assert!(
            render.lines().any(|l| l == line),
            "missing {line}\n{render}"
        );
    }
    assert!(!task.is_finished(), "the supervisor keeps running");
    assert!(
        !render.contains("tv_api_token_reload_respawn_total"),
        "the inner loop did not exit"
    );
    // No read happened, so the loaded token is unchanged.
    assert_eq!(config.token_value_for_test(), token);
    task.abort();
}
