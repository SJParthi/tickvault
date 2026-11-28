//! Coverage for `tickvault_app::brutex_export_boot` (the daily Brutex export
//! scheduler) and `tickvault_app::daily_archive_boot` (the daily partition
//! archive scheduler).
//!
//! Both modules put every scheduling decision in small pure functions (export
//! window, session-day steps, visit order, hold and gate reasons, last-chance
//! and forced-day logic, the archive pass verdict, attempt ledger, latch and
//! config decisions). CI's coverage report showed most of them unexecuted by
//! any test the workspace coverage run counts, so this file pins each one
//! against exact expected values, boundaries (exactly at a threshold, one
//! second either side), malformed input and properties that must hold for any
//! input. No network, no AWS, no wall clock: every date and time is explicit,
//! and this test binary installs no trading calendar, so `session_info` falls
//! back to Monday to Friday.

use chrono::{NaiveDate, TimeZone as _};
use proptest::prelude::*;
use tickvault_app::brutex_export_boot::{
    BLOCKS_EVEN_FORCED, CATCH_UP_DAYS, DISK_STOP_RETRY_SECS, DayState, DayStep,
    EXPORT_GIVE_UP_AFTER_SECS, EXPORT_OPEN_SECS_IST, EXPORT_SOFT_WAIT_SECS, Gates, HARD_REASONS,
    LoopCarry, MAX_ATTEMPTS_PER_DAY, NOTE_REASON_PREFIXES, PAST_DAY_FORCE_AFTER_DAYS,
    RETRY_BACKOFF_SECS, RunEnd, WAL_REPLAY_HOLDS_FORCED, apply_run_end, blocks_even_forced,
    book_panicked_run, candle_final_target_secs, candles_final, compose_reasons,
    days_leaving_window, days_leaving_window_with_released, delays, eligibility_day,
    export_floor_bytes, export_open_secs, export_pending, export_running, export_window_open,
    first_export_day, has_hard_reason, last_window_chance, last_window_chance_with,
    left_window_report_wanted, oldest_window_day, past_day_forced, past_day_step, ready,
    retry_backoff_secs, session_day_of, session_day_step, spawn_brutex_export_loop, visit_order,
    visit_order_with_released, within_session_horizon, yield_to_session_day,
};
use tickvault_app::contract_name_repair_boot::RepairDayState;
use tickvault_app::daily_archive_boot::{
    ARCHIVE_WINDOW_END_SECS_IST, ArchiveTick, AttemptLedger, LatchDecision,
    MAX_DAILY_ARCHIVE_ATTEMPTS, PassOutcome, PassVerdict, SEEDED_ATTEMPT_OUTCOMES,
    archive_session_day, archive_tick, daily_latch_decision, daily_pass_config,
    daily_pass_stop_unix_secs, pass_verdict, process_saw_whole_session, refunds_attempt,
    should_measure_session, spawn_supervised_daily_archive_loop, window_open_secs_of_day,
};
use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::market_hours::SessionDayInfo;
use tickvault_storage::partition_archive::ArchiveRunSummary;

const DAY: u32 = 86_400;
const GIB: u64 = 1024 * 1024 * 1024;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

fn trading() -> SessionDayInfo {
    SessionDayInfo {
        trading: true,
        muhurat_end_secs: None,
    }
}

fn quiet_gates() -> Gates {
    Gates {
        seal_applied: None,
        candles_final: None,
        repair: None,
        repair_running: false,
        arrival_hours_open: None,
        wal_backlog: Some(0),
        evening_replay_pending: false,
        depth_pass_floor_blocked: false,
    }
}

fn state(attempts: u32, retry_at_unix: i64) -> DayState {
    DayState {
        attempts,
        retry_at_unix,
        sealed: false,
        gave_up: false,
    }
}

fn questdb() -> QuestDbConfig {
    QuestDbConfig {
        host: "tv-questdb".to_string(),
        http_port: 9000,
        pg_port: 8812,
        ilp_port: 9009,
    }
}

// ---------------------------------------------------------------------------
// brutex_export_boot: constants and small pure helpers
// ---------------------------------------------------------------------------

#[test]
fn test_first_export_day_is_quote_60_date() {
    assert_eq!(first_export_day(), d(2026, 11, 14));
}

#[test]
fn test_export_constants_hold_their_documented_values() {
    assert_eq!(EXPORT_SOFT_WAIT_SECS, 8_100);
    assert_eq!(EXPORT_GIVE_UP_AFTER_SECS, 18_000);
    assert_eq!(MAX_ATTEMPTS_PER_DAY, 3);
    assert_eq!(RETRY_BACKOFF_SECS, 900);
    assert_eq!(CATCH_UP_DAYS, 7);
    assert_eq!(PAST_DAY_FORCE_AFTER_DAYS, 2);
    assert_eq!(DISK_STOP_RETRY_SECS, 900);
    assert_eq!(WAL_REPLAY_HOLDS_FORCED, "wal_backlog_replaying");
    assert!(HARD_REASONS.contains(&WAL_REPLAY_HOLDS_FORCED));
    // The repair-running block is also hard; the pending block is soft.
    assert!(HARD_REASONS.contains(&BLOCKS_EVEN_FORCED[0]));
    assert!(!HARD_REASONS.contains(&BLOCKS_EVEN_FORCED[1]));
    // No NOTE prefix ever matches a hard reason.
    for hard in HARD_REASONS {
        assert!(
            !NOTE_REASON_PREFIXES.iter().any(|p| hard.starts_with(p)),
            "{hard} must not be a note"
        );
    }
}

#[test]
fn test_session_day_of_belongs_to_previous_day_before_0730() {
    let cases = [
        // (date, secs) -> (session day, secs into it)
        ((d(2026, 11, 25), 0), (d(2026, 11, 24), DAY)),
        ((d(2026, 11, 25), 300), (d(2026, 11, 24), DAY + 300)),
        (
            (d(2026, 11, 25), 27_000 - 1),
            (d(2026, 11, 24), DAY + 26_999),
        ),
        ((d(2026, 11, 25), 27_000), (d(2026, 11, 25), 27_000)),
        ((d(2026, 11, 25), 57_000), (d(2026, 11, 25), 57_000)),
        ((d(2026, 1, 1), 300), (d(2025, 12, 31), DAY + 300)),
        ((d(2024, 3, 1), 10), (d(2024, 2, 29), DAY + 10)),
    ];
    for ((date, secs), expected) in cases {
        assert_eq!(session_day_of(date, secs), expected, "{date} {secs}");
    }
    // The first date chrono knows has no previous day: read as today.
    assert_eq!(session_day_of(NaiveDate::MIN, 5), (NaiveDate::MIN, 5));
}

#[test]
fn test_candle_final_target_is_the_hourly_bar_end_on_each_kind_of_day() {
    // A trading day: the hourly 15:15 bar ends at 16:15.
    assert_eq!(
        candle_final_target_secs(trading()),
        Some(16 * 3600 + 15 * 60)
    );
    // No session at all.
    assert_eq!(
        candle_final_target_secs(SessionDayInfo {
            trading: false,
            muhurat_end_secs: None,
        }),
        None
    );
    // A Muhurat-only day ending 19:30: the 19:15 hourly bar ends 20:15.
    let muhurat = SessionDayInfo {
        trading: false,
        muhurat_end_secs: Some(19 * 3600 + 30 * 60),
    };
    assert_eq!(candle_final_target_secs(muhurat), Some(20 * 3600 + 15 * 60));
    // A trading day that is also a Muhurat day: the later end wins.
    let both = SessionDayInfo {
        trading: true,
        muhurat_end_secs: Some(19 * 3600 + 30 * 60),
    };
    assert_eq!(candle_final_target_secs(both), Some(20 * 3600 + 15 * 60));
    // A Muhurat session ending before 15:40 on a trading day: 15:40 wins.
    let early = SessionDayInfo {
        trading: true,
        muhurat_end_secs: Some(14 * 3600),
    };
    assert_eq!(candle_final_target_secs(early), Some(16 * 3600 + 15 * 60));
    // A Muhurat end at second 0: every bucket clamps to the 09:15 grid
    // start, so the hourly bucket 09:15-10:15 is the latest end.
    let zero = SessionDayInfo {
        trading: false,
        muhurat_end_secs: Some(0),
    };
    assert_eq!(candle_final_target_secs(zero), Some(10 * 3600 + 15 * 60));
}

#[test]
fn test_candles_final_reads_the_floor_against_the_day_start() {
    let day_start = 1_000_000_i64;
    let target = 58_500_u32;
    // No progress: no live lane folded in this process.
    assert!(candles_final(None, day_start, target));
    // Exactly at the target.
    assert!(candles_final(Some((1_058_500, 7)), day_start, target));
    // One second short.
    assert!(!candles_final(Some((1_058_499, 7)), day_start, target));
    // Past it.
    assert!(candles_final(Some((2_000_000, 0)), day_start, target));
    // A floor before the day started never counts.
    assert!(!candles_final(Some((10, 0)), day_start, target));
    // Target 0 with the floor at the day start is final.
    assert!(candles_final(Some((1_000_000, 0)), day_start, 0));
    // A negative day start never overflows.
    assert!(candles_final(Some((0, 0)), i64::MIN, u32::MAX));
}

#[test]
fn test_retry_backoff_doubles_and_caps_at_256_times() {
    let cases = [
        (0_u32, 900_i64),
        (1, 900),
        (2, 1_800),
        (3, 3_600),
        (4, 7_200),
        (9, 900 * 256),
        (10, 900 * 256),
        (u32::MAX, 900 * 256),
    ];
    for (attempts, expected) in cases {
        assert_eq!(
            retry_backoff_secs(attempts),
            expected,
            "attempts {attempts}"
        );
    }
}

#[test]
fn test_hard_reasons_are_exact_labels_and_notes_are_prefixes() {
    for hard in HARD_REASONS {
        assert!(has_hard_reason(&s(&[hard])), "{hard}");
        assert!(delays(hard));
        assert!(!ready(&s(&[hard])));
    }
    // Exact labels only: a longer or shorter label is not hard.
    assert!(!has_hard_reason(&s(&["seal_not_drained_x", "candles"])));
    assert!(!has_hard_reason(&[]));
    assert!(!has_hard_reason(&s(&["contract_names_repair_pending"])));

    let notes = [
        "wal_backlog:5",
        "wal_backlog:",
        "wal_backlog_unreadable",
        "deferred_depth_marks_floor_blocked",
        "contract_names_repair_failed",
        "contract_names_repair_failed_and_more",
    ];
    for note in notes {
        assert!(!delays(note), "{note} must not delay");
    }
    assert!(ready(&s(&notes)));
    assert!(ready(&[]));
    // Soft reasons delay but are not hard.
    for soft in [
        "spill_files",
        "contract_names_repair_pending",
        "",
        "wal_backlog",
    ] {
        assert!(delays(soft), "{soft:?} delays");
        assert!(!has_hard_reason(&s(&[soft])));
    }
    assert!(!ready(&s(&["wal_backlog:3", "spill_files"])));
}

#[test]
fn test_blocks_even_forced_table() {
    let running = "contract_names_repair_running";
    let pending = "contract_names_repair_pending";
    let replay = WAL_REPLAY_HOLDS_FORCED;
    // (reasons, after_midnight, last_chance, expected)
    let cases: [(&[&str], bool, bool, bool); 12] = [
        (&[], false, false, false),
        (&[running], false, false, true),
        (&[running], true, true, true),
        (&[pending], false, false, true),
        (&[pending], true, false, false),
        (&[pending], false, true, true),
        (&[replay], false, false, true),
        (&[replay], true, false, true),
        (&[replay], false, true, false),
        (&[replay, pending], true, true, false),
        (&["wal_backlog:5", "spill_files"], false, false, false),
        (&["contract_names_unrepaired"], false, false, false),
    ];
    for (reasons, after_midnight, last_chance, expected) in cases {
        assert_eq!(
            blocks_even_forced(&s(reasons), after_midnight, last_chance),
            expected,
            "{reasons:?} after_midnight={after_midnight} last_chance={last_chance}"
        );
    }
}

// ---------------------------------------------------------------------------
// compose_reasons
// ---------------------------------------------------------------------------

#[test]
fn test_compose_reasons_quiet_gates_add_nothing() {
    assert!(compose_reasons(Vec::new(), quiet_gates()).is_empty());
    assert_eq!(
        compose_reasons(s(&["spill_files"]), quiet_gates()),
        s(&["spill_files"])
    );
}

#[test]
fn test_compose_reasons_relabels_deferred_depth_marks() {
    let base = s(&["deferred_depth_marks"]);
    assert_eq!(
        compose_reasons(base.clone(), quiet_gates()),
        s(&["deferred_depth_pass_running"])
    );
    let blocked = Gates {
        depth_pass_floor_blocked: true,
        ..quiet_gates()
    };
    let out = compose_reasons(base, blocked);
    assert_eq!(out, s(&["deferred_depth_marks_floor_blocked"]));
    // The floor-blocked label is a NOTE, the running one is HARD.
    assert!(ready(&out));
    assert!(has_hard_reason(&s(&["deferred_depth_pass_running"])));
}

#[test]
fn test_compose_reasons_live_side_gates() {
    let gates = Gates {
        seal_applied: Some(false),
        candles_final: Some(false),
        repair_running: true,
        arrival_hours_open: Some(true),
        ..quiet_gates()
    };
    assert_eq!(
        compose_reasons(Vec::new(), gates),
        s(&[
            "arrival_hours_open",
            "candles_not_final",
            "contract_names_repair_running",
            "seal_not_drained",
        ])
    );
    let all_good = Gates {
        seal_applied: Some(true),
        candles_final: Some(true),
        arrival_hours_open: Some(false),
        ..quiet_gates()
    };
    assert!(compose_reasons(Vec::new(), all_good).is_empty());
}

#[test]
fn test_compose_reasons_repair_states() {
    let cases = [
        (
            Some((RepairDayState::Pending, true)),
            vec!["contract_names_unrepaired"],
        ),
        (
            Some((RepairDayState::Pending, false)),
            vec!["contract_names_repair_pending"],
        ),
        (
            Some((RepairDayState::GaveUpWithFailures, true)),
            vec!["contract_names_repair_failed"],
        ),
        (
            Some((RepairDayState::GaveUpWithFailures, false)),
            vec!["contract_names_repair_failed"],
        ),
        (Some((RepairDayState::Clean, true)), vec![]),
        (Some((RepairDayState::Clean, false)), vec![]),
        (None, vec![]),
    ];
    for (repair, expected) in cases {
        let gates = Gates {
            repair,
            ..quiet_gates()
        };
        assert_eq!(
            compose_reasons(Vec::new(), gates),
            s(&expected),
            "{repair:?}"
        );
    }
}

#[test]
fn test_compose_reasons_wal_backlog_labels() {
    let pending = Gates {
        wal_backlog: Some(3),
        evening_replay_pending: true,
        ..quiet_gates()
    };
    assert_eq!(
        compose_reasons(Vec::new(), pending),
        s(&["wal_backlog_replaying"])
    );
    let stuck = Gates {
        wal_backlog: Some(7),
        ..quiet_gates()
    };
    assert_eq!(compose_reasons(Vec::new(), stuck), s(&["wal_backlog:7"]));
    let max = Gates {
        wal_backlog: Some(u64::MAX),
        ..quiet_gates()
    };
    assert_eq!(
        compose_reasons(Vec::new(), max),
        vec![format!("wal_backlog:{}", u64::MAX)]
    );
    let unreadable = Gates {
        wal_backlog: None,
        evening_replay_pending: true,
        ..quiet_gates()
    };
    assert_eq!(
        compose_reasons(Vec::new(), unreadable),
        s(&["wal_backlog_unreadable"])
    );
    // Zero backlog with a pending replay still adds nothing.
    let zero = Gates {
        wal_backlog: Some(0),
        evening_replay_pending: true,
        ..quiet_gates()
    };
    assert!(compose_reasons(Vec::new(), zero).is_empty());
}

#[test]
fn test_compose_reasons_sorts_and_dedups() {
    let base = s(&["zeta", "seal_not_drained", "alpha", "zeta"]);
    let gates = Gates {
        seal_applied: Some(false),
        ..quiet_gates()
    };
    assert_eq!(
        compose_reasons(base, gates),
        s(&["alpha", "seal_not_drained", "zeta"])
    );
}

// ---------------------------------------------------------------------------
// forced day, last chance, window leaving
// ---------------------------------------------------------------------------

#[test]
fn test_last_window_chance_with_custom_calendars() {
    let session_day = d(2026, 11, 25);
    // No later session day at all: the day is at its last chance.
    assert!(last_window_chance_with(
        d(2026, 11, 24),
        session_day,
        |_| false
    ));
    // Every day a session day: a recent day has later chances.
    assert!(!last_window_chance_with(
        d(2026, 11, 24),
        session_day,
        |_| true
    ));
    // The oldest window day always is at its last chance.
    assert!(last_window_chance_with(
        d(2026, 11, 19),
        session_day,
        |_| true
    ));
    // Older than the window: no later day keeps it, last chance.
    assert!(last_window_chance_with(d(2026, 11, 1), session_day, |_| {
        true
    }));
    // Only a session day beyond the day's window: last chance.
    // Day 11-24's window ends at 11-30 (11-30 - 6 = 11-24); 12-01 is out.
    assert!(last_window_chance_with(d(2026, 11, 24), session_day, |x| {
        x == d(2026, 12, 1)
    }));
    // A session day exactly at the window edge keeps it.
    assert!(!last_window_chance_with(
        d(2026, 11, 24),
        session_day,
        |x| { x == d(2026, 11, 30) }
    ));
    // The latest date chrono knows: no later day can be formed.
    assert!(last_window_chance_with(
        NaiveDate::MAX,
        NaiveDate::MAX,
        |_| true
    ));
}

#[test]
fn test_last_window_chance_on_the_weekday_fallback_calendar() {
    // Wednesday 2026-11-25; Thursday 11-26 is a later session day.
    assert!(!last_window_chance(d(2026, 11, 24), d(2026, 11, 25)));
    assert!(last_window_chance(d(2026, 11, 19), d(2026, 11, 25)));
    // Friday 2026-11-27, day Sunday 11-22: later window days 11-28 (Sat)
    // and nothing else (11-29 - 6 = 11-23 > 11-22): last chance.
    assert!(last_window_chance(d(2026, 11, 22), d(2026, 11, 27)));
    // Friday 2026-11-27, day Tuesday 11-24: Monday 11-30 keeps it.
    assert!(!last_window_chance(d(2026, 11, 24), d(2026, 11, 27)));
}

#[test]
fn test_past_day_forced_by_age_or_last_chance() {
    let session_day = d(2026, 11, 25);
    assert!(!past_day_forced(d(2026, 11, 24), session_day));
    assert!(past_day_forced(d(2026, 11, 23), session_day));
    assert!(past_day_forced(d(2026, 11, 19), session_day));
    // The session day itself is neither old nor at its last chance.
    assert!(!past_day_forced(session_day, session_day));
    // Friday session day, Thursday: 1 day old, Monday keeps it.
    assert!(!past_day_forced(d(2026, 11, 26), d(2026, 11, 27)));
}

#[test]
fn test_days_leaving_window_lists_session_days_from_the_first_export_day() {
    // Saturday 2026-11-28: back 7..13 is 11-21..11-15; weekends dropped.
    assert_eq!(
        days_leaving_window(d(2026, 11, 28)),
        vec![
            d(2026, 11, 20),
            d(2026, 11, 19),
            d(2026, 11, 18),
            d(2026, 11, 17),
            d(2026, 11, 16),
        ]
    );
    // Before the first export day nothing is reported.
    assert!(days_leaving_window(d(2026, 11, 20)).is_empty());
    assert!(days_leaving_window(d(2026, 1, 5)).is_empty());
    // Monday 2026-12-07.
    assert_eq!(
        days_leaving_window(d(2026, 12, 7)),
        vec![
            d(2026, 11, 30),
            d(2026, 11, 27),
            d(2026, 11, 26),
            d(2026, 11, 25),
            d(2026, 11, 24),
        ]
    );
}

#[test]
fn test_days_leaving_window_with_released_adds_only_eligible_released_days() {
    let session_day = d(2026, 12, 7);
    let released = [
        // Eligible from 11-27, inside [11-24, 11-30], a Monday: added.
        (d(2026, 11, 16), d(2026, 11, 27)),
        // Eligible from 12-05: still inside its window, not added.
        (d(2026, 11, 17), d(2026, 12, 5)),
        // Already listed: no duplicate.
        (d(2026, 11, 25), d(2026, 11, 20)),
        // A Sunday: no session, not added.
        (d(2026, 11, 15), d(2026, 11, 27)),
        // Before the first export day: not added.
        (d(2026, 11, 10), d(2026, 11, 27)),
        // Eligible before the lookback: not added.
        (d(2026, 11, 18), d(2026, 11, 18)),
    ];
    assert_eq!(
        days_leaving_window_with_released(session_day, &released),
        vec![
            d(2026, 11, 30),
            d(2026, 11, 27),
            d(2026, 11, 26),
            d(2026, 11, 25),
            d(2026, 11, 24),
            d(2026, 11, 16),
        ]
    );
    assert_eq!(
        days_leaving_window_with_released(session_day, &[]),
        days_leaving_window(session_day)
    );
    // A session day so early that the window bounds cannot be formed.
    assert!(days_leaving_window_with_released(NaiveDate::MIN, &released).is_empty());
}

// ---------------------------------------------------------------------------
// run ends and steps
// ---------------------------------------------------------------------------

#[test]
fn test_apply_run_end_each_variant() {
    let now = 1_000_000_i64;

    let mut sealed = state(2, 5);
    apply_run_end(&mut sealed, RunEnd::Sealed, now);
    assert_eq!(
        sealed,
        DayState {
            attempts: 2,
            retry_at_unix: 5,
            sealed: true,
            gave_up: false
        }
    );

    let mut window = state(2, 5);
    apply_run_end(&mut window, RunEnd::StoppedWindow, now);
    assert_eq!(window, state(1, 5));

    let mut window_zero = state(0, 5);
    apply_run_end(&mut window_zero, RunEnd::StoppedWindow, now);
    assert_eq!(window_zero, state(0, 5));

    let mut disk = state(1, 5);
    apply_run_end(&mut disk, RunEnd::StoppedDisk, now);
    assert_eq!(disk, state(0, now + DISK_STOP_RETRY_SECS));

    let mut failed = state(2, 5);
    apply_run_end(&mut failed, RunEnd::Failed, now);
    assert_eq!(failed, state(2, now + 1_800));

    let mut failed_max = state(3, 0);
    apply_run_end(&mut failed_max, RunEnd::Failed, i64::MAX);
    assert_eq!(failed_max.retry_at_unix, i64::MAX);
}

#[test]
fn test_session_day_step_table() {
    let open = EXPORT_OPEN_SECS_IST;
    let soft_end = open + EXPORT_SOFT_WAIT_SECS;
    let now = 10_000_i64;
    let clean = state(0, 0);
    let hard = s(&["seal_not_drained"]);
    let soft = s(&["spill_files"]);
    let note = s(&["wal_backlog:4"]);
    let cases: [(u32, DayState, &[String], DayStep); 13] = [
        (open - 1, clean, &[], DayStep::NotDue),
        (open - 1, state(9, i64::MAX), &hard, DayStep::NotDue),
        (open, clean, &[], DayStep::Run),
        (open, state(3, 0), &[], DayStep::GiveUp),
        (open, state(2, 0), &[], DayStep::Run),
        (open, state(1, now + 1), &[], DayStep::Wait),
        (open, state(1, now), &[], DayStep::Run),
        (soft_end + 1_000, clean, &hard, DayStep::Wait),
        (open, clean, &soft, DayStep::Wait),
        (soft_end - 1, clean, &soft, DayStep::Wait),
        (soft_end, clean, &soft, DayStep::Run),
        (open, clean, &note, DayStep::Run),
        (open + DAY, clean, &soft, DayStep::Run),
    ];
    for (secs, st, reasons, expected) in cases {
        assert_eq!(
            session_day_step(secs, open, st, now, reasons),
            expected,
            "secs={secs} state={st:?} reasons={reasons:?}"
        );
    }
}

#[test]
fn test_within_session_horizon_boundary() {
    let open = EXPORT_OPEN_SECS_IST;
    assert!(within_session_horizon(open, open));
    assert!(within_session_horizon(0, open));
    assert!(within_session_horizon(
        open + EXPORT_GIVE_UP_AFTER_SECS - 1,
        open
    ));
    assert!(!within_session_horizon(
        open + EXPORT_GIVE_UP_AFTER_SECS,
        open
    ));
    // A saturating open never wraps into "inside".
    assert!(within_session_horizon(u32::MAX - 1, u32::MAX));
    assert!(!within_session_horizon(u32::MAX, u32::MAX));
}

#[test]
fn test_past_day_step_table() {
    let now = 500_i64;
    // (state, ready, forced, blocked, expected)
    let cases = [
        (state(3, 0), true, true, false, DayStep::GiveUp),
        (state(3, 0), false, false, true, DayStep::GiveUp),
        (state(0, now + 1), true, true, false, DayStep::Wait),
        (state(0, 0), true, true, true, DayStep::Wait),
        (state(0, 0), true, false, false, DayStep::Run),
        (state(0, 0), false, true, false, DayStep::Run),
        (state(0, 0), false, false, false, DayStep::Wait),
        (state(2, now), false, true, false, DayStep::Run),
    ];
    for (st, is_ready, forced, blocked, expected) in cases {
        assert_eq!(
            past_day_step(st, now, is_ready, forced, blocked),
            expected,
            "{st:?} ready={is_ready} forced={forced} blocked={blocked}"
        );
    }
}

#[test]
fn test_yield_to_session_day_table() {
    let all = [
        DayStep::NotDue,
        DayStep::Wait,
        DayStep::Run,
        DayStep::GiveUp,
    ];
    for step in all {
        for pending in [false, true] {
            for last in [false, true] {
                let expected = if step == DayStep::Run && pending && !last {
                    DayStep::Wait
                } else {
                    step
                };
                assert_eq!(yield_to_session_day(step, pending, last), expected);
            }
        }
    }
    assert_eq!(
        yield_to_session_day(DayStep::Run, true, false),
        DayStep::Wait
    );
    assert_eq!(yield_to_session_day(DayStep::Run, true, true), DayStep::Run);
}

// ---------------------------------------------------------------------------
// visit order and released days
// ---------------------------------------------------------------------------

#[test]
fn test_visit_order_is_session_day_first_then_oldest_first() {
    assert_eq!(
        visit_order(d(2026, 11, 25)),
        vec![
            d(2026, 11, 25),
            d(2026, 11, 19),
            d(2026, 11, 20),
            d(2026, 11, 21),
            d(2026, 11, 22),
            d(2026, 11, 23),
            d(2026, 11, 24),
        ]
    );
    assert_eq!(oldest_window_day(d(2026, 11, 25)), Some(d(2026, 11, 19)));
    // Across a year end.
    assert_eq!(oldest_window_day(d(2026, 1, 3)), Some(d(2025, 12, 28)));
    // The first date chrono knows: no earlier day exists.
    assert_eq!(visit_order(NaiveDate::MIN), vec![NaiveDate::MIN]);
    assert_eq!(oldest_window_day(NaiveDate::MIN), None);
}

#[test]
fn test_eligibility_day_uses_the_later_of_day_and_done_date() {
    let released = [
        (d(2026, 11, 20), d(2026, 11, 28)),
        (d(2026, 11, 10), d(2026, 11, 5)),
    ];
    assert_eq!(eligibility_day(d(2026, 11, 20), &released), d(2026, 11, 28));
    // A done date before the day (clock step) never moves it earlier.
    assert_eq!(eligibility_day(d(2026, 11, 10), &released), d(2026, 11, 10));
    // Not released.
    assert_eq!(eligibility_day(d(2026, 11, 21), &released), d(2026, 11, 21));
    assert_eq!(eligibility_day(d(2026, 11, 21), &[]), d(2026, 11, 21));
}

#[test]
fn test_visit_order_with_released_inserts_after_the_session_day() {
    let session_day = d(2026, 11, 25);
    let released = [
        // Left the window, done inside it: added.
        (d(2026, 11, 10), d(2026, 11, 20)),
        // Done before the window: not added.
        (d(2026, 11, 12), d(2026, 11, 18)),
        // Done after the session day (clock step): not added.
        (d(2026, 11, 11), d(2026, 11, 26)),
        // Still inside the window by its own date: not duplicated.
        (d(2026, 11, 20), d(2026, 11, 22)),
        // Done exactly on the session day: added.
        (d(2026, 11, 1), d(2026, 11, 25)),
        // Done exactly on the oldest window day: added.
        (d(2026, 11, 2), d(2026, 11, 19)),
    ];
    let mut expected = vec![session_day, d(2026, 11, 10), d(2026, 11, 1), d(2026, 11, 2)];
    expected.extend(visit_order(session_day).into_iter().skip(1));
    assert_eq!(visit_order_with_released(session_day, &released), expected);
    assert_eq!(
        visit_order_with_released(session_day, &[]),
        visit_order(session_day)
    );
    assert_eq!(
        visit_order_with_released(NaiveDate::MIN, &released),
        vec![NaiveDate::MIN]
    );
}

#[test]
fn test_left_window_report_wanted_table() {
    let session_day = d(2026, 11, 25);
    // Oldest window day is 11-19.
    assert!(left_window_report_wanted(
        d(2026, 11, 18),
        session_day,
        false,
        &[]
    ));
    assert!(!left_window_report_wanted(
        d(2026, 11, 19),
        session_day,
        false,
        &[]
    ));
    // Held: never reported.
    assert!(!left_window_report_wanted(
        d(2026, 11, 10),
        session_day,
        true,
        &[]
    ));
    // Released with a done date inside the window: not yet.
    let released = [(d(2026, 11, 10), d(2026, 11, 22))];
    assert!(!left_window_report_wanted(
        d(2026, 11, 10),
        session_day,
        false,
        &released
    ));
    // Released with a done date before the window: reported.
    let old_release = [(d(2026, 11, 10), d(2026, 11, 12))];
    assert!(left_window_report_wanted(
        d(2026, 11, 10),
        session_day,
        false,
        &old_release
    ));
    // No window can be formed: always wanted unless held.
    assert!(left_window_report_wanted(
        NaiveDate::MIN,
        NaiveDate::MIN,
        false,
        &[]
    ));
}

// ---------------------------------------------------------------------------
// panic bookkeeping, disk floor, export window, spawn
// ---------------------------------------------------------------------------

#[test]
fn test_book_panicked_run_books_a_failed_attempt() {
    let mut carry = LoopCarry::default();
    let day = d(2026, 11, 25);
    carry.running = Some((day, state(2, 0)));
    carry.days.insert(day, state(1, 0));
    book_panicked_run(&mut carry, 1_000);
    assert!(carry.running.is_none());
    assert_eq!(carry.days.get(&day), Some(&state(2, 1_000 + 1_800)));
    assert_eq!(carry.days.len(), 1);

    // Nothing running: nothing changes.
    let other = d(2026, 11, 24);
    carry.days.insert(other, state(1, 7));
    book_panicked_run(&mut carry, 2_000);
    assert_eq!(carry.days.get(&other), Some(&state(1, 7)));
    assert_eq!(carry.days.get(&day), Some(&state(2, 2_800)));
    assert_eq!(carry.days.len(), 2);
}

#[test]
fn test_export_floor_bytes_is_the_larger_floor() {
    // An empty volume: the after-close minimum of 10 GiB.
    assert_eq!(export_floor_bytes(0, 75), 10 * GIB);
    // 150 GiB at a 75% high water: the pressure line (25%) wins.
    let total = 150 * GIB;
    assert_eq!(export_floor_bytes(total, 75), total / 4);
    // At 95% high water: 5% + 9 GiB (after-close floor) wins over 5%.
    assert_eq!(export_floor_bytes(total, 95), total / 20 + 9 * GIB);
    // At 100% the pressure line is zero; above 100 is clamped.
    assert_eq!(export_floor_bytes(total, 100), total / 20 + 9 * GIB);
    assert_eq!(export_floor_bytes(total, 255), total / 20 + 9 * GIB);
    // At 0% the whole volume must be free.
    assert_eq!(export_floor_bytes(total, 0), total);
    // A huge volume never overflows.
    assert_eq!(export_floor_bytes(u64::MAX, 0), u64::MAX);
}

#[test]
fn test_reexported_export_window_matches_the_trading_day_rules() {
    assert_eq!(export_open_secs(trading()), Some(EXPORT_OPEN_SECS_IST));
    let none = SessionDayInfo {
        trading: false,
        muhurat_end_secs: None,
    };
    assert_eq!(export_open_secs(none), None);
    let forbidden_from = 8 * 3600 + 30 * 60;
    // (secs, open?)
    let cases = [
        (0_u32, true),
        (forbidden_from - 1, true),
        (forbidden_from, false),
        (EXPORT_OPEN_SECS_IST - 1, false),
        (EXPORT_OPEN_SECS_IST, true),
        (DAY - 1, true),
    ];
    for (secs, expected) in cases {
        assert_eq!(export_window_open(trading(), secs), expected, "{secs}");
        // A day with no session never forbids.
        assert!(export_window_open(none, secs));
    }
}

#[test]
fn test_spawn_brutex_export_loop_is_off_when_disabled() {
    let cfg = PartitionRetentionConfig::default();
    assert!(!cfg.brutex_export_enabled);
    assert!(spawn_brutex_export_loop(questdb(), cfg).is_none());
    // Nothing armed: neither flag is set.
    assert!(!export_running());
    assert!(!export_pending());
}

// ---------------------------------------------------------------------------
// daily_archive_boot
// ---------------------------------------------------------------------------

#[test]
fn test_archive_tick_and_outcome_labels() {
    assert_eq!(ArchiveTick::Run.as_str(), "run");
    assert_eq!(ArchiveTick::BeforeWindow.as_str(), "before_window");
    assert_eq!(ArchiveTick::AlreadyCompleted.as_str(), "already_completed");
    assert_eq!(
        ArchiveTick::AttemptsExhausted.as_str(),
        "attempts_exhausted"
    );

    assert_eq!(PassOutcome::Ran(PassVerdict::Complete).as_str(), "complete");
    assert_eq!(
        PassOutcome::Ran(PassVerdict::Incomplete("quiesce_busy")).as_str(),
        "quiesce_busy"
    );
    assert_eq!(PassOutcome::Skipped.as_str(), "skipped");
    assert_eq!(PassOutcome::Failed.as_str(), "failed");

    assert!(PassOutcome::Ran(PassVerdict::Complete).latches_the_day());
    assert!(!PassOutcome::Ran(PassVerdict::Incomplete("contended")).latches_the_day());
    assert!(!PassOutcome::Skipped.latches_the_day());
    assert!(!PassOutcome::Failed.latches_the_day());
}

fn ran() -> ArchiveRunSummary {
    ArchiveRunSummary {
        pass_ran: true,
        ..ArchiveRunSummary::default()
    }
}

#[test]
fn test_pass_verdict_order_and_each_reason() {
    let cap = 200_u32;
    let complete = PassVerdict::Complete;
    let inc = PassVerdict::Incomplete;
    let cases: Vec<(ArchiveRunSummary, PassVerdict)> = vec![
        // An all-zero summary never ran.
        (ArchiveRunSummary::default(), inc("did_not_run")),
        // Contended beats did-not-run.
        (
            ArchiveRunSummary {
                contended: true,
                ..ArchiveRunSummary::default()
            },
            inc("contended"),
        ),
        (ran(), complete),
        (
            ArchiveRunSummary {
                tables_wal_suspended: 1,
                failed: 3,
                ..ran()
            },
            inc("wal_suspended"),
        ),
        (
            ArchiveRunSummary {
                tables_list_failed: 1,
                failed: 2,
                tables_wal_not_applied: 1,
                ..ran()
            },
            inc("table_list_failed"),
        ),
        (
            ArchiveRunSummary {
                failed: 1,
                tables_wal_not_applied: 1,
                ..ran()
            },
            inc("partition_failed"),
        ),
        (
            ArchiveRunSummary {
                tables_wal_not_applied: 1,
                partitions_considered: 500,
                ..ran()
            },
            inc("wal_not_applied"),
        ),
        // Exactly at the cap: truncated.
        (
            ArchiveRunSummary {
                partitions_considered: 200,
                ..ran()
            },
            inc("budget_exhausted"),
        ),
        // One under the cap.
        (
            ArchiveRunSummary {
                partitions_considered: 199,
                ..ran()
            },
            complete,
        ),
        // Held partitions take no budget: 230 - 31 = 199.
        (
            ArchiveRunSummary {
                partitions_considered: 230,
                held_deferred_depth: 10,
                held_quiesce_busy: 11,
                held_in_flight: 10,
                ..ran()
            },
            inc("deferred_depth_pass"),
        ),
        (
            ArchiveRunSummary {
                held_deferred_depth: 1,
                held_quiesce_busy: 1,
                ..ran()
            },
            inc("deferred_depth_pass"),
        ),
        (
            ArchiveRunSummary {
                held_quiesce_busy: 2,
                ..ran()
            },
            inc("quiesce_busy"),
        ),
        // In flight alone does not keep the day open.
        (
            ArchiveRunSummary {
                held_in_flight: 9,
                partitions_considered: 9,
                ..ran()
            },
            complete,
        ),
        // Absent tables never block.
        (
            ArchiveRunSummary {
                tables_absent: 19,
                ..ran()
            },
            complete,
        ),
    ];
    for (summary, expected) in cases {
        assert_eq!(pass_verdict(&summary, cap), expected, "{summary:?}");
    }
    // A cap of zero means no budget check at all.
    let huge = ArchiveRunSummary {
        partitions_considered: u32::MAX,
        ..ran()
    };
    assert_eq!(pass_verdict(&huge, 0), complete);
    assert_eq!(pass_verdict(&huge, 1), inc("budget_exhausted"));
}

#[test]
fn test_refunds_attempt_only_for_stand_asides_and_wal_lag() {
    for reason in [
        "contended",
        "deferred_depth_pass",
        "quiesce_busy",
        "wal_not_applied",
    ] {
        assert!(refunds_attempt(PassOutcome::Ran(PassVerdict::Incomplete(
            reason
        ))));
    }
    for reason in [
        "did_not_run",
        "wal_suspended",
        "table_list_failed",
        "partition_failed",
        "budget_exhausted",
    ] {
        assert!(!refunds_attempt(PassOutcome::Ran(PassVerdict::Incomplete(
            reason
        ))));
    }
    assert!(!refunds_attempt(PassOutcome::Ran(PassVerdict::Complete)));
    assert!(!refunds_attempt(PassOutcome::Skipped));
    assert!(!refunds_attempt(PassOutcome::Failed));
}

#[test]
fn test_seeded_outcomes_cover_every_label_the_verdict_can_produce() {
    let mut seen = std::collections::HashSet::new();
    for label in SEEDED_ATTEMPT_OUTCOMES {
        assert!(seen.insert(label), "duplicate {label}");
    }
    let summaries = [
        ArchiveRunSummary::default(),
        ArchiveRunSummary {
            contended: true,
            ..ArchiveRunSummary::default()
        },
        ran(),
        ArchiveRunSummary {
            tables_wal_suspended: 1,
            ..ran()
        },
        ArchiveRunSummary {
            tables_list_failed: 1,
            ..ran()
        },
        ArchiveRunSummary { failed: 1, ..ran() },
        ArchiveRunSummary {
            tables_wal_not_applied: 1,
            ..ran()
        },
        ArchiveRunSummary {
            partitions_considered: 5,
            ..ran()
        },
        ArchiveRunSummary {
            held_deferred_depth: 1,
            ..ran()
        },
        ArchiveRunSummary {
            held_quiesce_busy: 1,
            ..ran()
        },
    ];
    for summary in summaries {
        let label = PassOutcome::Ran(pass_verdict(&summary, 5)).as_str();
        assert!(seen.contains(label), "{label} not seeded");
    }
    assert!(seen.contains(PassOutcome::Skipped.as_str()));
    assert!(seen.contains(PassOutcome::Failed.as_str()));
}

#[test]
fn test_attempt_ledger_records_refunds_and_rolls_over() {
    let mut ledger = AttemptLedger::default();
    assert_eq!(ledger.attempts_on(100), 0);
    // A refund with no record is a no-op.
    ledger.refund(100);
    assert_eq!(ledger.attempts_on(100), 0);

    ledger.record(100);
    ledger.record(100);
    ledger.record(100);
    assert_eq!(ledger.attempts_on(100), 3);
    assert_eq!(ledger.attempts_on(101), 0);

    // A refund for another day does nothing.
    ledger.refund(99);
    assert_eq!(ledger.attempts_on(100), 3);
    ledger.refund(100);
    assert_eq!(ledger.attempts_on(100), 2);
    ledger.refund(100);
    ledger.refund(100);
    ledger.refund(100);
    assert_eq!(ledger.attempts_on(100), 0);

    // A new day resets the count to one.
    ledger.record(100);
    ledger.record(101);
    assert_eq!(ledger.attempts_on(101), 1);
    assert_eq!(ledger.attempts_on(100), 0);
    // Negative day keys work like any other.
    ledger.record(-5);
    assert_eq!(ledger.attempts_on(-5), 1);
    assert_eq!(ledger.attempts_on(101), 0);
}

#[test]
fn test_archive_tick_table() {
    let open = 55_802_u32;
    let max = MAX_DAILY_ARCHIVE_ATTEMPTS;
    assert_eq!(max, 6);
    let cases = [
        // (now, completed, attempts, expected)
        (0_u32, true, 99, ArchiveTick::AlreadyCompleted),
        (open, true, 0, ArchiveTick::AlreadyCompleted),
        (open - 1, false, 0, ArchiveTick::BeforeWindow),
        (open - 1, false, max, ArchiveTick::BeforeWindow),
        (open, false, max, ArchiveTick::AttemptsExhausted),
        (open, false, max + 1, ArchiveTick::AttemptsExhausted),
        (open, false, max - 1, ArchiveTick::Run),
        (DAY + 100, false, 0, ArchiveTick::Run),
    ];
    for (now, completed, attempts, expected) in cases {
        assert_eq!(
            archive_tick(now, open, completed, attempts, max),
            expected,
            "now={now} completed={completed} attempts={attempts}"
        );
    }
    // A zero budget is exhausted at once.
    assert_eq!(
        archive_tick(open, open, false, 0, 0),
        ArchiveTick::AttemptsExhausted
    );
}

#[test]
fn test_window_open_secs_of_day_parses_or_refuses() {
    let cases = [
        ("15:30:00", Some(55_802)),
        ("00:00:00", Some(2)),
        ("23:59:57", Some(86_399)),
        // Would open at midnight or later: refused.
        ("23:59:58", None),
        ("23:59:59", None),
        ("15:30", None),
        ("garbage", None),
        ("", None),
        ("25:00:00", None),
        ("15:30:00 ", None),
        ("-1:00:00", None),
    ];
    for (input, expected) in cases {
        assert_eq!(window_open_secs_of_day(input), expected, "{input:?}");
    }
}

#[test]
fn test_archive_session_day_table() {
    assert_eq!(ARCHIVE_WINDOW_END_SECS_IST, 27_000);
    let cases = [
        ((d(2026, 11, 26), 0), (d(2026, 11, 25), DAY)),
        ((d(2026, 11, 26), 26_999), (d(2026, 11, 25), DAY + 26_999)),
        ((d(2026, 11, 26), 27_000), (d(2026, 11, 26), 27_000)),
        ((d(2026, 11, 26), 86_399), (d(2026, 11, 26), 86_399)),
        ((d(2027, 1, 1), 1), (d(2026, 12, 31), DAY + 1)),
    ];
    for ((date, secs), expected) in cases {
        assert_eq!(archive_session_day(date, secs), expected, "{date} {secs}");
    }
    assert_eq!(archive_session_day(NaiveDate::MIN, 3), (NaiveDate::MIN, 3));
}

#[test]
fn test_daily_pass_stop_is_0730_ist_next_day() {
    let expected = chrono::Utc
        .with_ymd_and_hms(2026, 11, 28, 2, 0, 0)
        .single()
        .map(|t| t.timestamp());
    assert_eq!(daily_pass_stop_unix_secs(d(2026, 11, 27)), expected);
    // Across a year end.
    let new_year = chrono::Utc
        .with_ymd_and_hms(2027, 1, 1, 2, 0, 0)
        .single()
        .map(|t| t.timestamp());
    assert_eq!(daily_pass_stop_unix_secs(d(2026, 12, 31)), new_year);
    // Consecutive days are exactly one day apart.
    let a = daily_pass_stop_unix_secs(d(2026, 3, 1)).unwrap_or_default();
    let b = daily_pass_stop_unix_secs(d(2026, 3, 2)).unwrap_or_default();
    assert_eq!(b - a, i64::from(DAY));
    // Past chrono's last date there is no next day.
    assert_eq!(daily_pass_stop_unix_secs(NaiveDate::MAX), None);
}

#[test]
fn test_daily_latch_decision_table() {
    let complete = PassOutcome::Ran(PassVerdict::Complete);
    let keep = |reason: &'static str, refund: bool| LatchDecision::KeepOpen { reason, refund };
    let cases = [
        (
            complete,
            0_u32,
            Some((100_u64, 100_u64)),
            LatchDecision::Latch,
        ),
        (complete, 0, Some((101, 100)), LatchDecision::Latch),
        (
            complete,
            0,
            Some((99, 100)),
            keep("below_free_target", true),
        ),
        (complete, 0, None, keep("free_space_unknown", true)),
        (
            complete,
            1,
            Some((500, 100)),
            keep("dropped_this_pass", true),
        ),
        (
            PassOutcome::Ran(PassVerdict::Incomplete("partition_failed")),
            0,
            Some((500, 100)),
            keep("partition_failed", false),
        ),
        (
            PassOutcome::Ran(PassVerdict::Incomplete("partition_failed")),
            2,
            None,
            keep("partition_failed", true),
        ),
        (
            PassOutcome::Ran(PassVerdict::Incomplete("contended")),
            0,
            None,
            keep("contended", true),
        ),
        (
            PassOutcome::Skipped,
            0,
            Some((500, 1)),
            keep("skipped", false),
        ),
        (
            PassOutcome::Failed,
            0,
            Some((500, 1)),
            keep("failed", false),
        ),
        (PassOutcome::Failed, 3, None, keep("failed", true)),
    ];
    for (outcome, dropped, free, expected) in cases {
        assert_eq!(
            daily_latch_decision(outcome, dropped, free),
            expected,
            "{outcome:?} dropped={dropped} free={free:?}"
        );
    }
}

#[test]
fn test_should_measure_session_boundaries() {
    let end = 56_400_u32;
    let cases = [
        (false, end - 1, true, false),
        (false, end, true, true),
        (false, DAY - 1, true, true),
        (false, DAY, true, false),
        (false, DAY + 100, true, false),
        (true, end, true, false),
        (false, end, false, false),
        (false, 0, true, false),
    ];
    for (measured, secs, saw_whole, expected) in cases {
        assert_eq!(
            should_measure_session(measured, secs, saw_whole),
            expected,
            "measured={measured} secs={secs} saw_whole={saw_whole}"
        );
    }
}

#[test]
fn test_process_saw_whole_session_at_and_after_the_open() {
    let day = d(2026, 11, 27);
    // 09:00 IST on 2026-11-27 is 03:30 UTC.
    let open = chrono::Utc
        .with_ymd_and_hms(2026, 11, 27, 3, 30, 0)
        .single()
        .map(|t| t.timestamp())
        .unwrap_or_default();
    assert!(process_saw_whole_session(open, day));
    assert!(process_saw_whole_session(open - 1, day));
    assert!(!process_saw_whole_session(open + 1, day));
    // Armed a day earlier.
    assert!(process_saw_whole_session(open - i64::from(DAY), day));
    // Armed the previous day's evening, measured for the next day.
    assert!(process_saw_whole_session(open, d(2026, 11, 28)));
    // Armed long after.
    assert!(!process_saw_whole_session(i64::MAX, day));
    assert!(process_saw_whole_session(i64::MIN, day));
}

#[test]
fn test_daily_pass_config_switches_to_the_hour_path_below_target() {
    let base = PartitionRetentionConfig {
        archive_enabled: true,
        max_partitions_per_run: 77,
        ..PartitionRetentionConfig::default()
    };
    assert!(base.market_depth_force_with_spill_pending);

    let above = daily_pass_config(&base, false);
    assert_eq!(above.under_disk_pressure, base.under_disk_pressure);
    assert!(above.market_depth_force_with_spill_pending);
    assert_eq!(above.max_partitions_per_run, 77);
    assert!(above.archive_enabled);

    let below = daily_pass_config(&base, true);
    assert!(below.under_disk_pressure);
    assert!(!below.market_depth_force_with_spill_pending);
    // Every other field is carried unchanged.
    assert_eq!(below.max_partitions_per_run, 77);
    assert!(below.archive_enabled);
    assert_eq!(below.brutex_export_enabled, base.brutex_export_enabled);
    assert_eq!(
        below.drain_hold_counts_archive,
        base.drain_hold_counts_archive
    );
    assert_eq!(
        below.pressure_archive_enabled,
        base.pressure_archive_enabled
    );
}

#[test]
fn test_spawn_daily_archive_loop_refuses_when_disabled_or_unparseable() {
    let dir = std::env::temp_dir().join(format!(
        "tv-cov-brutex-archive-{}-spawn",
        std::process::id()
    ));
    let disabled = PartitionRetentionConfig::default();
    assert!(!disabled.archive_enabled);
    assert!(
        spawn_supervised_daily_archive_loop(
            dir.clone(),
            questdb(),
            disabled,
            "15:30:00".to_string(),
            None,
        )
        .is_none()
    );
    // Enabled but with a close time that cannot be parsed: refused, not
    // defaulted to midnight.
    let enabled = PartitionRetentionConfig {
        archive_enabled: true,
        ..PartitionRetentionConfig::default()
    };
    for bad in ["15:30", "nope", "23:59:59"] {
        assert!(
            spawn_supervised_daily_archive_loop(
                dir.clone(),
                questdb(),
                enabled.clone(),
                bad.to_string(),
                None,
            )
            .is_none(),
            "{bad}"
        );
    }
    // Nothing was written to the folder.
    assert!(!dir.exists());
}

// ---------------------------------------------------------------------------
// properties
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn test_prop_session_day_step_invariants(
        secs in 0_u32..(2 * DAY),
        open in 0_u32..DAY,
        attempts in 0_u32..6,
        retry_at in -10_i64..10,
        now in -10_i64..10,
        hard in any::<bool>(),
        soft in any::<bool>(),
    ) {
        let mut reasons = Vec::new();
        if hard { reasons.push("candles_not_final".to_string()); }
        if soft { reasons.push("spill_files".to_string()); }
        let step = session_day_step(secs, open, state(attempts, retry_at), now, &reasons);
        prop_assert_eq!(step == DayStep::NotDue, secs < open);
        if secs >= open && attempts >= MAX_ATTEMPTS_PER_DAY {
            prop_assert_eq!(step, DayStep::GiveUp);
        }
        if hard {
            prop_assert_ne!(step, DayStep::Run);
        }
        if now < retry_at {
            prop_assert_ne!(step, DayStep::Run);
        }
    }

    #[test]
    fn test_prop_past_day_step_blocked_never_runs(
        attempts in 0_u32..6,
        ready_flag in any::<bool>(),
        forced in any::<bool>(),
        blocked in any::<bool>(),
    ) {
        let step = past_day_step(state(attempts, 0), 0, ready_flag, forced, blocked);
        if attempts >= MAX_ATTEMPTS_PER_DAY {
            prop_assert_eq!(step, DayStep::GiveUp);
        } else if blocked {
            prop_assert_eq!(step, DayStep::Wait);
        } else {
            prop_assert_eq!(step == DayStep::Run, ready_flag || forced);
        }
    }

    #[test]
    fn test_prop_visit_order_shape(offset in 0_i64..20_000) {
        let session_day = d(2000, 1, 1) + chrono::TimeDelta::days(offset);
        let days = visit_order(session_day);
        prop_assert_eq!(days.len(), usize::try_from(CATCH_UP_DAYS).unwrap_or(0));
        prop_assert_eq!(days[0], session_day);
        prop_assert_eq!(Some(days[1]), oldest_window_day(session_day));
        for pair in days[1..].windows(2) {
            prop_assert_eq!(pair[1] - pair[0], chrono::TimeDelta::days(1));
        }
        prop_assert!(days[1..].iter().all(|x| *x < session_day));
    }

    #[test]
    fn test_prop_candle_target_brackets_the_session_end(end in 33_301_u32..DAY) {
        let info = SessionDayInfo { trading: false, muhurat_end_secs: Some(end) };
        let target = candle_final_target_secs(info).unwrap_or(0);
        prop_assert!(target >= end, "target {} end {}", target, end);
        prop_assert!(target < end + 3_600, "target {} end {}", target, end);
    }

    #[test]
    fn test_prop_archive_session_day_secs_are_continuous(
        offset in 1_i64..20_000,
        secs in 0_u32..DAY,
    ) {
        let date = d(2000, 1, 1) + chrono::TimeDelta::days(offset);
        let (session, into) = archive_session_day(date, secs);
        if secs >= ARCHIVE_WINDOW_END_SECS_IST {
            prop_assert_eq!((session, into), (date, secs));
        } else {
            prop_assert_eq!(session, date - chrono::TimeDelta::days(1));
            prop_assert_eq!(into, secs + DAY);
        }
        prop_assert_eq!(session_day_of(date, secs), (session, into));
    }

    #[test]
    fn test_prop_compose_reasons_is_sorted_and_unique(
        base in proptest::collection::vec("[a-z_]{0,12}", 0..8),
        backlog in proptest::option::of(0_u64..5),
        replay in any::<bool>(),
        seal in proptest::option::of(any::<bool>()),
    ) {
        let gates = Gates {
            seal_applied: seal,
            wal_backlog: backlog,
            evening_replay_pending: replay,
            ..quiet_gates()
        };
        let out = compose_reasons(base.clone(), gates);
        for pair in out.windows(2) {
            prop_assert!(pair[0] < pair[1]);
        }
        for reason in &base {
            if reason != "deferred_depth_marks" {
                prop_assert!(out.contains(reason));
            }
        }
        prop_assert_eq!(out.contains(&"seal_not_drained".to_string()), seal == Some(false)
            || base.iter().any(|r| r == "seal_not_drained"));
    }
}
