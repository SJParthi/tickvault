//! Coverage for `dhan_live_crossverify_boot::spawn_dhan_live_crossverify` and
//! `live_side_readiness`, against a QuestDB-shaped server on 127.0.0.1 and a
//! paused tokio clock.
//!
//! A test binary has no Dhan token manager, so every attempt of the day ends
//! `no_token` (or is skipped for time after 16:23 IST) and the day's final
//! failure pages once, which writes today's "paged" marker. That marker, in a
//! temp working directory, is the observable effect asserted on a weekday; on
//! a weekend the task skips the day and writes nothing. Nothing reaches Dhan:
//! the intraday URL points at the local server and is never called without a
//! token.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use cov_xvs_support::{
    CwdGuard, FakeQuestDb, captured_events, drive_paused, init_no_proxy, init_tracing, is_weekday,
    ist_now, lock_cwd, weekday_calendar,
};
use tickvault_app::daily_task_marker::daily_marker_path;
use tickvault_app::dhan_live_crossverify::DhanLiveCrossverifyConfig;
use tickvault_app::dhan_live_crossverify_boot::{
    CROSSVERIFY_MARKER_TASK, CROSSVERIFY_PAGED_MARKER_TASK, Completeness, CrossverifyBootDeps,
    Durability, LiveReadiness, live_side_readiness, spawn_dhan_live_crossverify,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;
use tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyNotReady;

fn midnight_ist_fold_nanos(day: chrono::NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
        .timestamp_nanos_opt()
        .expect("in range")
}

#[tokio::test(start_paused = true)]
async fn test_dhan_live_crossverify_boot_live_side_readiness_past_day_with_no_progress_reads_unknown()
 {
    init_no_proxy();
    init_tracing();
    // It lists the seal spill folder by a relative path.
    let _cwd_lock = lock_cwd().await;
    let srv = FakeQuestDb::start(Arc::new(|_q: &str| {
        (200, r#"{"columns":[],"dataset":[]}"#.to_owned())
    }));
    let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).expect("date");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    let started = tokio::time::Instant::now();
    let readiness =
        live_side_readiness(&srv.exec_url(), day, midnight_ist_fold_nanos(day), deadline).await;
    // No catch-up sweep and no seal drain in this process: the seals are
    // pending and nothing tells the session's last minutes were sealed.
    assert_eq!(
        readiness,
        LiveReadiness {
            durability: Durability::NotReady(DhanLiveXverifyNotReady::SealsPending),
            completeness: Completeness::Unknown,
        }
    );
    // It waits for the deadline (it cannot decide earlier) and no longer.
    let waited = tokio::time::Instant::now() - started;
    assert!(waited >= Duration::from_secs(12), "{waited:?}");
    assert!(waited < Duration::from_secs(20), "{waited:?}");
}

#[tokio::test(start_paused = true)]
async fn test_dhan_live_crossverify_boot_live_side_readiness_deadline_already_passed_returns_at_once()
 {
    init_no_proxy();
    init_tracing();
    // It lists the seal spill folder by a relative path.
    let _cwd_lock = lock_cwd().await;
    let srv = FakeQuestDb::start_default();
    let day = chrono::NaiveDate::from_ymd_opt(2025, 12, 31).expect("date");
    let deadline = tokio::time::Instant::now();
    let readiness =
        live_side_readiness(&srv.exec_url(), day, midnight_ist_fold_nanos(day), deadline).await;
    assert_eq!(readiness.completeness, Completeness::Unknown);
    assert!(matches!(readiness.durability, Durability::NotReady(_)));
}

/// `error!` sources the day's final failure pages on (the three alarmed
/// `xverify_*` sources).
const PAGE_SOURCES: [&str; 3] = ["xverify_failed", "xverify_vacuous", "xverify_diverged"];

fn page_lines() -> usize {
    captured_events()
        .iter()
        .filter(|e| {
            e.level == tracing::Level::ERROR
                && e.field("source").is_some_and(|s| PAGE_SOURCES.contains(&s))
        })
        .count()
}

fn armed_lines() -> usize {
    captured_events()
        .iter()
        .filter(|e| e.message().contains("cross-verification armed"))
        .count()
}

#[tokio::test(start_paused = true)]
async fn test_dhan_live_crossverify_boot_spawn_without_a_token_pages_the_day_once_on_a_weekday() {
    init_no_proxy();
    init_tracing();
    let _cwd_lock = lock_cwd().await;
    let cwd = CwdGuard::enter("crossverify-spawn");
    let pages_before = page_lines();
    let armed_before = armed_lines();

    let srv = FakeQuestDb::start_default();
    let deps = CrossverifyBootDeps {
        questdb_exec_url: srv.exec_url(),
        intraday_url: format!("http://127.0.0.1:{}/v2/charts/intraday", srv.port),
        config: DhanLiveCrossverifyConfig {
            tolerance_paise: 0,
            fetch_timeout_secs: 1,
            live_read_timeout_secs: 1,
            run_budget_secs: 600,
        },
        questdb: srv.questdb_config(),
        calendar: weekday_calendar(),
    };
    // One index target and one F&O contract, which cannot be targeted.
    let main_feed = [
        SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::IdxI,
        },
        SubscribeInstrument {
            security_id: 52_001,
            segment: ExchangeSegment::NseFno,
        },
    ];
    let (today, _) = ist_now();
    let paged = cwd
        .path()
        .join(daily_marker_path(CROSSVERIFY_PAGED_MARKER_TASK, today));
    let recorded = cwd
        .path()
        .join(daily_marker_path(CROSSVERIFY_MARKER_TASK, today));

    let handle = spawn_dhan_live_crossverify(deps, &main_feed);
    // 20 h of virtual time reaches the day's last attempt (the run starts at
    // 15:41 IST and every attempt ends by 16:23) from any start; the loop
    // stops as soon as the paged marker shows.
    let paged_seen = drive_paused(&srv, Duration::from_secs(20 * 3_600), || paged.exists()).await;
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");

    let recorded_exists = recorded.exists();
    let requests = srv.request_lines();
    let pages = page_lines() - pages_before;
    let armed = armed_lines() - armed_before;
    drop(cwd);

    // Every day: armed once, nothing recorded, the vendor never called.
    assert_eq!(armed, 1, "the task logs that it is armed exactly once");
    assert!(
        !recorded_exists,
        "with no token nothing is compared, so the day is never recorded"
    );
    assert!(
        !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
        "the vendor is never called without a token: {requests:?}"
    );
    // Whatever the day, the page happens at most once (the paged marker).
    assert!(pages <= 1, "at most one page per day: {pages}");
    let (end_day, _) = ist_now();
    if end_day != today {
        return; // crossed IST midnight during the test
    }
    if is_weekday(today) {
        assert!(paged_seen, "a failed weekday pages once and notes it");
        assert_eq!(pages, 1, "exactly one page line on a failed weekday");
    } else {
        assert!(!paged_seen, "a weekend is skipped, nothing is paged");
        assert_eq!(pages, 0, "no page line on a weekend");
    }
}
