//! Behavioral regressions for missing-minute/signal/calendar evidence.

use super::tests::{at, evidence_pass, good, run_day};
use super::*;
use tickvault_common::config::{MuhuratTradingEntry, NseHolidayEntry, TradingConfig};

fn trading() -> TradingConfig {
    let config: toml::Value =
        toml::from_str(include_str!("../../../../config/base.toml")).expect("config");
    config["trading"].clone().try_into().expect("trading")
}

#[test]
fn test_day_verdict_strict_failure_survives_1024_bound_and_incomplete_session() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let (mut rows, host) = good(at(0, 9, 15));
    let row = &mut rows[Stage::DrainMainFrame as usize];
    row.strict_version = Some(1);
    row.strict_samples = Some(100);
    row.strict_samples_at_or_above_1000ns = Some(100);
    assert_eq!(row.p99_ns, Some(1_024));
    assert!(verdict.observe(&rows, &host).is_none());
    let (rows, host) = good(at(0, 15, 40));
    let daily = verdict
        .observe(&rows, &host)
        .expect("incomplete daily evidence");
    assert_eq!(daily.tick_step.0, SignalResult::Fail);
    assert_eq!(daily.verdict, Verdict::Unknown);
    let reasons = verdict_reasons(&daily);
    assert!(reasons.contains("FAIL strict_1000ns_observed"));
    assert!(reasons.contains("drain_main_frame"));
    assert!(!reasons.contains("UNKNOWN strict_1000ns_unresolved"));
}

#[test]
fn test_day_verdict_strict_passing_counts_do_not_certify_session_intervals() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let row = run_day(&mut verdict, 0, |minute| {
        let (mut rows, host) = good(minute);
        for row in &mut rows {
            row.strict_version = Some(1);
            row.strict_samples = Some(100);
            row.strict_samples_at_or_above_1000ns = Some(0);
        }
        (rows, host)
    })
    .expect("daily row");
    assert_eq!(row.coverage.strict_sampled_minutes, [375; 4]);
    assert_eq!(row.tick_step.0, SignalResult::Unknown);
    assert_eq!(row.verdict, Verdict::Unknown);
    assert!(verdict_reasons(&row).contains("strict_session_intervals_unproven"));
}

pub(super) fn calendar() -> Arc<TradingCalendar> {
    Arc::new(TradingCalendar::from_config(&trading()).expect("calendar"))
}

fn with_session(start: &str, end: &str) -> Arc<TradingCalendar> {
    let mut config = trading();
    config.muhurat_trading_dates = vec![MuhuratTradingEntry {
        date: "2026-11-08".to_owned(),
        name: "fixture session".to_owned(),
        session_start_ist: start.to_owned(),
        session_end_ist: end.to_owned(),
    }];
    Arc::new(TradingCalendar::from_config(&config).expect("calendar"))
}

#[test]
fn test_session_plan_calendar_and_muhurat() {
    let cal = calendar();
    assert_eq!(
        SessionPlan::for_day(Some(&cal), at(0, 0, 0)),
        SessionPlan::Open {
            start: 9 * 3600 + 15 * 60,
            end: 15 * 3600 + 30 * 60,
            due: 15 * 3600 + 40 * 60,
        }
    );
    assert_eq!(
        SessionPlan::for_day(Some(&cal), at(5, 0, 0)),
        SessionPlan::Closed
    );
    // Config is the source, including its explicit assumption status; the
    // fixture pins the current afternoon session rather than an evening guess.
    let plan = SessionPlan::for_day(Some(&cal), at(6, 0, 0));
    assert_eq!(plan.coverage().expected_minutes, Some(95));
    assert_eq!(plan.coverage().verdict_at_secs, Some(15 * 3600 + 50 * 60));
    assert!(!plan.contains(14 * 3600 + 4 * 60));
    assert!(plan.contains(14 * 3600 + 5 * 60));
    assert!(plan.contains(15 * 3600 + 39 * 60));
    assert!(!plan.contains(15 * 3600 + 40 * 60));

    let mut config = trading();
    config.nse_holidays.push(NseHolidayEntry {
        date: "2026-11-03".to_owned(),
        name: "fixture holiday".to_owned(),
    });
    config.nse_mock_trading_dates.push(NseHolidayEntry {
        date: "2026-11-07".to_owned(),
        name: "fixture mock".to_owned(),
    });
    let cal = TradingCalendar::from_config(&config).expect("calendar");
    assert_eq!(
        SessionPlan::for_day(Some(&cal), at(1, 0, 0)),
        SessionPlan::Closed
    );
    assert_eq!(
        SessionPlan::for_day(Some(&cal), at(5, 0, 0)),
        SessionPlan::Closed
    );
    assert_eq!(
        SessionPlan::for_day(None, at(0, 0, 0)),
        SessionPlan::Unknown("calendar_missing")
    );
    assert_eq!(
        SessionPlan::for_day(Some(&cal), -1),
        SessionPlan::Unknown("calendar_invalid_date")
    );
    assert_eq!(
        SessionPlan::for_day(Some(&cal), at(365, 0, 0)),
        SessionPlan::Unknown("calendar_stale_or_empty")
    );
    config.nse_holidays.clear();
    let empty = TradingCalendar::from_config(&config).expect("empty calendar");
    assert_eq!(
        SessionPlan::for_day(Some(&empty), at(0, 0, 0)),
        SessionPlan::Unknown("calendar_stale_or_empty")
    );
    let unknown = with_session("", "");
    assert_eq!(
        SessionPlan::for_day(Some(&unknown), at(6, 0, 0)),
        SessionPlan::Unknown("muhurat_times_unknown")
    );
    let invalid = with_session("19:00", "20:00");
    assert_eq!(
        SessionPlan::for_day(Some(&invalid), at(6, 0, 0)),
        SessionPlan::Unknown("muhurat_times_invalid")
    );
}

#[test]
fn test_day_verdict_one_good_minute_cannot_hide_empty_minutes() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let row = run_day(&mut verdict, 0, |minute| {
        let (mut latency, host) = good(minute);
        if minute != at(0, 9, 15) {
            for stage in TICK_STEP_STAGES {
                let row = &mut latency[stage as usize];
                row.samples = 0;
                row.p99_ns = None;
            }
        }
        (latency, host)
    })
    .expect("daily row");
    assert_eq!(row.session_minutes, 375);
    assert_eq!(row.coverage.stage_minutes, [1; 4]);
    assert_eq!(row.tick_step.0, SignalResult::Unknown);
    assert_eq!(row.ticks_over_1ms.0, SignalResult::Unknown);
    assert_eq!(row.verdict, Verdict::Unknown);
    assert!(verdict_reasons(&row).contains("UNKNOWN stage_samples_incomplete"));
}

#[test]
fn test_day_verdict_each_missing_signal_is_unknown() {
    for missing in 0..7 {
        let mut verdict = DayVerdict::with_calendar(calendar());
        let row = run_day(&mut verdict, 0, |minute| {
            let (mut latency, mut host) = good(minute);
            if minute == at(0, 12, 1) {
                match missing {
                    0..=3 => latency[TICK_STEP_STAGES[missing] as usize].samples = 0,
                    4 => host.reader_wait = None,
                    5 => host.drain_wait = None,
                    _ => host.disk_read_bytes_per_s = None,
                }
            }
            (latency, host)
        })
        .expect("daily row");
        assert_eq!(row.verdict, Verdict::Unknown, "missing {missing}");
        let affected = match missing {
            0..=3 => row.tick_step.0,
            4..=5 => row.cpu_wait.0,
            _ => row.disk.0,
        };
        assert_eq!(affected, SignalResult::Unknown, "missing {missing}");
    }
}

#[test]
fn test_day_verdict_distinct_minutes_and_malformed_rows() {
    for minutes in [1, 300, 374] {
        let mut verdict = DayVerdict::with_calendar(calendar());
        for offset in 0..minutes {
            let (latency, host) = good(at(0, 9, 15) + offset * 60 * NANOS_PER_SECOND);
            assert!(verdict.observe(&latency, &host).is_none());
            // Duplicates neither inflate coverage nor fill a different minute.
            assert!(verdict.observe(&latency, &host).is_none());
        }
        let (latency, host) = good(at(0, 15, 40));
        let row = verdict.observe(&latency, &host).expect("daily row");
        assert_eq!(row.session_minutes, minutes as u64);
        assert_eq!(row.verdict, Verdict::Unknown);
        assert_eq!(
            row.cpu_wait.0,
            SignalResult::Unknown,
            "whole session has missing minutes"
        );
        assert!(verdict_reasons(&row).contains("session_minutes_incomplete"));
    }
    for malformed in 0..5 {
        let mut verdict = DayVerdict::with_calendar(calendar());
        let row = run_day(&mut verdict, 0, |minute| {
            let (mut latency, mut host) = good(minute);
            if minute == at(0, 12, 0) {
                match malformed {
                    0 => latency[0].minute_ist_nanos += 60 * NANOS_PER_SECOND,
                    1 => latency[0].stage = Stage::RingDwell,
                    2 => latency[0].publishes = 0,
                    3 => host.sched_seconds = 0,
                    _ => host.minute_ist_nanos += 1,
                }
            }
            (latency, host)
        })
        .expect("daily row");
        assert_eq!(row.session_minutes, 374, "malformed {malformed}");
        assert_eq!(row.coverage.malformed_minutes, 1);
        assert_eq!(row.verdict, Verdict::Unknown);
    }
}

#[test]
fn test_day_verdict_59_reads_are_valid_observations_not_a_time_guarantee() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let row = run_day(&mut verdict, 0, |minute| {
        let (mut latency, mut host) = good(minute);
        for stage in &mut latency {
            stage.publishes = 59;
        }
        host.sched_seconds = 59;
        (latency, host)
    })
    .expect("daily row");
    assert_eq!(row.session_minutes, 375);
    assert_eq!(row.coverage.malformed_minutes, 0);
    assert_eq!(row.coverage.stage_minutes, [375; 4]);
    assert_eq!(row.cpu_wait.0, SignalResult::Pass);
    // No assertion of continuous one-second coverage: no interval metadata exists.
}

#[test]
fn test_day_verdict_observed_exclusions_are_incomplete() {
    for excluded in 0..3 {
        let mut verdict = DayVerdict::with_calendar(calendar());
        let row = run_day(&mut verdict, 0, |minute| {
            let (latency, mut host) = good(minute);
            if minute == at(0, 10, 0) {
                match excluded {
                    0 => host.ring_refused_frames = 1,
                    1 => host.ticks_not_landed = Some(1),
                    _ => host.ticks_lost = Some(1),
                }
            }
            (latency, host)
        })
        .expect("daily row");
        assert_eq!(row.verdict, Verdict::Unknown);
        assert_eq!(row.tick_step.0, SignalResult::Unknown);
        assert_eq!(row.ticks_over_1ms.0, SignalResult::Unknown);
        assert_eq!(row.tick_step.2, Some(1024), "keep measured bound");
        assert!(verdict_reasons(&row).contains("UNKNOWN live_path_incomplete"));
    }
}

#[test]
fn test_day_verdict_zero_minutes_and_post_close_restart() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let (latency, host) = good(at(0, 8, 50));
    assert!(verdict.observe(&latency, &host).is_none());
    let (latency, host) = good(at(0, 15, 40));
    let row = verdict
        .observe(&latency, &host)
        .expect("empty session is visible");
    assert_eq!(row.session_minutes, 0);
    assert_eq!(row.verdict, Verdict::Unknown);
    let mut restarted = DayVerdict::with_calendar(calendar());
    assert!(
        restarted.observe(&latency, &host).is_none(),
        "do not replace existing dedup row"
    );
    let (latency, host) = good(at(1, 0, 0));
    assert!(
        restarted.observe(&latency, &host).is_none(),
        "post-close-only prior day stays untouched"
    );
}

#[test]
fn test_day_verdict_with_calendar_special_session_and_unknown_calendar() {
    let mut verdict = DayVerdict::with_calendar(with_session("18:00", "19:15"));
    let (latency, host) = good(at(6, 15, 40));
    assert!(
        verdict.observe(&latency, &host).is_none(),
        "evening has not happened"
    );
    for offset in 0..75 {
        let (latency, host) = good(at(6, 18, 0) + offset * 60 * NANOS_PER_SECOND);
        assert!(verdict.observe(&latency, &host).is_none());
    }
    let (latency, host) = good(at(6, 19, 25));
    let row = verdict
        .observe(&latency, &host)
        .expect("special-session verdict");
    assert_eq!(row.session_minutes, 75);
    assert_eq!(row.coverage.expected_minutes, Some(75));
    assert_eq!(row.coverage.stage_minutes, [75; 4]);
    assert_eq!(row.cpu_wait.0, SignalResult::Pass);
    assert_eq!(row.backlog.0, SignalResult::Pass);
    let mut unknown = DayVerdict::new();
    let (latency, host) = good(at(0, 8, 50));
    assert!(unknown.observe(&latency, &host).is_none());
    let (latency, host) = good(at(1, 0, 0));
    let row = unknown
        .observe(&latency, &host)
        .expect("unknown calendar at rollover");
    assert_eq!(row.verdict, Verdict::Unknown);
    assert!(verdict_reasons(&row).contains("UNKNOWN calendar_missing"));
    let mut unknown_after_close = DayVerdict::new();
    let (latency, host) = good(at(0, 16, 0));
    assert!(unknown_after_close.observe(&latency, &host).is_none());
    let (latency, host) = good(at(1, 0, 0));
    assert!(
        unknown_after_close.observe(&latency, &host).is_none(),
        "unknown schedule must not overwrite a completed day"
    );
}

#[test]
fn test_day_verdict_closed_days_and_late_backlog_do_not_pass() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    assert!(
        run_day(&mut verdict, 5, good).is_none(),
        "Saturday is not a regular session"
    );
    let mut verdict = DayVerdict::with_calendar(calendar());
    for offset in 0..375 {
        let (latency, host) = good(at(0, 9, 15) + offset * 60 * NANOS_PER_SECOND);
        assert!(verdict.observe(&latency, &host).is_none());
    }
    let (latency, host) = good(at(0, 16, 0));
    let row = verdict.observe(&latency, &host).expect("late verdict");
    assert_eq!(row.backlog, (SignalResult::Unknown, None, None));
    assert_eq!(row.verdict, Verdict::Unknown);
    assert!(verdict_reasons(&row).contains("UNKNOWN backlog_close_sample_missing"));
}

#[test]
fn test_wait_sum_any_missing_attempt_stays_unknown_until_next_minute() {
    for missing_first in [false, true] {
        let mut sum = WaitSum::default();
        if missing_first {
            sum.add(None);
        }
        sum.add(Some(7));
        if !missing_first {
            sum.add(None);
        }
        assert_eq!(
            sum.take(),
            None,
            "a later read cannot fill an earlier failed interval"
        );
        sum.add(Some(0));
        sum.add(Some(9));
        assert_eq!(sum.take(), Some((9, 9)), "next minute can recover");
    }
}

#[test]
fn test_day_verdict_incomplete_coverage_preserves_observed_breaches() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let row = run_day(&mut verdict, 0, |minute| {
        let (mut latency, mut host) = good(minute);
        if minute == at(0, 10, 0) {
            latency[Stage::DrainMainFrame as usize].p99_ns = Some(4096);
        }
        if minute == at(0, 10, 1) {
            latency[Stage::RingDwell as usize].samples = 0;
        }
        if minute == at(0, 10, 2) {
            host.reader_wait = Some((CPU_WAIT_FAIL_NS_PER_S, CPU_WAIT_FAIL_NS_PER_S));
        }
        if minute == at(0, 10, 3) {
            host.reader_wait = None;
        }
        (latency, host)
    })
    .expect("daily row");
    assert_eq!(row.verdict, Verdict::Unknown);
    assert_eq!(row.tick_step.0, SignalResult::Fail);
    assert_eq!(row.cpu_wait.0, SignalResult::Fail);
    assert!(verdict_reasons(&row).contains("UNKNOWN reader_wait_incomplete"));
}

#[test]
fn test_decide_verdict_strict_target_never_accepts_ambiguous_legacy_bucket() {
    for bound in [1000, 1001, 1024] {
        let mut evidence = evidence_pass();
        evidence.worst_step_p99 = [Some(bound); 4];
        assert_eq!(
            decide_verdict(&evidence).1[0],
            SignalResult::Unknown,
            "bound {bound}"
        );
        assert_eq!(decide_verdict(&evidence).0, Verdict::Unknown);
    }
    let mut evidence = evidence_pass();
    assert_eq!(
        decide_verdict(&evidence).1[0],
        SignalResult::Pass,
        "a discriminating999ns bound is sufficient"
    );
    evidence.worst_step_p99[2] = Some(1280);
    assert_eq!(
        decide_verdict(&evidence).1[0],
        SignalResult::Fail,
        "historical later bucket demonstrates a miss"
    );
    let mut verdict = DayVerdict::with_calendar(calendar());
    let row = run_day(&mut verdict, 0, good).expect("daily row");
    assert_eq!(row.tick_step.0, SignalResult::Unknown);
    assert!(verdict_reasons(&row).contains("UNKNOWN strict_1000ns_unresolved"));
}

#[test]
fn test_day_verdict_zero_accepted_rows_do_not_prove_zero_eligible_events() {
    let mut verdict = DayVerdict::with_calendar(calendar());
    let row = run_day(&mut verdict, 0, |minute| {
        let (mut latency, mut host) = good(minute);
        for stage in TICK_STEP_STAGES {
            latency[stage as usize].samples = 0;
        }
        host.ticks_accepted = Some(0);
        (latency, host)
    })
    .expect("daily row");
    assert_eq!(row.coverage.stage_minutes, [0; 4]);
    assert_eq!(row.tick_step.2, None);
    assert_eq!(row.tick_step.0, SignalResult::Unknown);
    assert_eq!(row.ticks_over_1ms.0, SignalResult::Unknown);
    assert_eq!(row.verdict, Verdict::Unknown);
}

#[test]
fn test_decide_verdict_each_coverage_gate_with_discriminating_bounds() {
    // Isolate coverage from the separate legacy-bucket ambiguity: these are
    // pure synthetic bounds, not a claim that current live histograms emit999.
    for missing in 0..8 {
        let mut evidence = evidence_pass();
        let affected = match missing {
            0..=3 => {
                evidence.coverage.stage_minutes[missing] -= 1;
                0
            }
            4 => {
                evidence.coverage.reader_minutes -= 1;
                2
            }
            5 => {
                evidence.coverage.drain_minutes -= 1;
                2
            }
            6 => {
                evidence.disk_minutes -= 1;
                3
            }
            _ => {
                evidence.session_minutes = 300;
                0
            }
        };
        let (verdict, signals) = decide_verdict(&evidence);
        assert_eq!(verdict, Verdict::Unknown, "missing {missing}");
        assert_eq!(
            signals[affected],
            SignalResult::Unknown,
            "missing {missing}"
        );
    }
    let mut evidence = evidence_pass();
    evidence.coverage.ring_refused_frames = 1;
    assert_eq!(decide_verdict(&evidence).1[0], SignalResult::Unknown);
    assert_eq!(decide_verdict(&evidence).1[1], SignalResult::Unknown);
}

#[test]
fn test_day_verdict_missing_exclusion_counters_are_unknown() {
    for missing_lost in [false, true] {
        for whole_day in [false, true] {
            let mut verdict = DayVerdict::with_calendar(calendar());
            let row = run_day(&mut verdict, 0, |minute| {
                let (latency, mut host) = good(minute);
                if whole_day || minute == at(0, 10, 0) {
                    if missing_lost {
                        host.ticks_lost = None;
                    } else {
                        host.ticks_not_landed = None;
                    }
                }
                (latency, host)
            })
            .expect("daily row");
            assert_eq!(
                row.coverage.exclusion_counter_minutes,
                if whole_day { 0 } else { 374 }
            );
            assert_eq!(row.ticks_over_1ms.0, SignalResult::Unknown);
            assert_eq!(row.verdict, Verdict::Unknown);
            assert!(verdict_reasons(&row).contains("UNKNOWN live_path_counters_incomplete"));
        }
    }
    let mut evidence = evidence_pass();
    evidence.coverage.exclusion_counter_minutes = 374;
    assert_eq!(
        decide_verdict(&evidence).1[0],
        SignalResult::Unknown,
        "discriminating bounds cannot fill missing loss evidence"
    );
}

#[test]
fn test_day_verdict_backlog_failure_requires_busy_evidence_to_rule_out_cores() {
    for missing in [0, 1, 2] {
        let mut verdict = DayVerdict::with_calendar(calendar());
        let row = run_day(&mut verdict, 0, |minute| {
            let (mut latency, mut host) = good(minute);
            // Known strict failure keeps the separate legacy-bucket ambiguity
            // out of this capacity inference test.
            latency[Stage::DrainMainFrame as usize].p99_ns = Some(4096);
            host.questdb_backlog_txns = Some(501);
            host.questdb_backlog_growing_tables = Some(1);
            if missing == 0 || minute == at(0, 10, 0) {
                host.host_cpu_busy_pct = if missing == 2 { Some(f64::NAN) } else { None };
            }
            (latency, host)
        })
        .expect("daily row");
        assert_eq!(
            row.coverage.host_busy_minutes,
            if missing == 0 { 0 } else { 374 }
        );
        assert_eq!(row.backlog.0, SignalResult::Fail);
        assert_eq!(row.verdict, Verdict::Unknown, "missing {missing}");
        assert!(verdict_reasons(&row).contains("UNKNOWN host_busy_incomplete"));
    }
    let mut evidence = evidence_pass();
    evidence.worst_step_p99 = [Some(4096); 4];
    evidence.backlog_at_close = (Some(501), Some(1));
    evidence.coverage.host_busy_minutes = 1;
    evidence.worst_host_busy_pct = Some(95.0);
    assert_eq!(
        decide_verdict(&evidence).0,
        Verdict::UpgradeNeeded,
        "observed high utilization is affirmative evidence"
    );
    evidence.coverage.host_busy_minutes = 0;
    evidence.worst_host_busy_pct = None;
    evidence.worst_reader_wait = Some(CPU_WAIT_FAIL_NS_PER_S);
    assert_eq!(
        decide_verdict(&evidence).0,
        Verdict::UpgradeNeeded,
        "independent CPU wait remains affirmative evidence"
    );
    evidence.worst_reader_wait = Some(0);
    evidence.backlog_at_close = (Some(0), Some(0));
    assert_eq!(
        decide_verdict(&evidence).0,
        Verdict::NotNeeded,
        "no backlog failure needs no busy inference"
    );
}
