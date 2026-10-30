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

use cov_xvs_support::{FakeQuestDb, TempRoot, init_tracing, is_weekday, ist_now, weekday_calendar};
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
    init_tracing();
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
    init_tracing();
    let srv = FakeQuestDb::start_default();
    let day = chrono::NaiveDate::from_ymd_opt(2025, 12, 31).expect("date");
    let deadline = tokio::time::Instant::now();
    let readiness =
        live_side_readiness(&srv.exec_url(), day, midnight_ist_fold_nanos(day), deadline).await;
    assert_eq!(readiness.completeness, Completeness::Unknown);
    assert!(matches!(readiness.durability, Durability::NotReady(_)));
}

#[tokio::test(start_paused = true)]
async fn test_dhan_live_crossverify_boot_spawn_without_a_token_pages_the_day_once_on_a_weekday() {
    init_tracing();
    let original_cwd = std::env::current_dir().expect("cwd");
    let root = TempRoot::new("crossverify-spawn");
    std::env::set_current_dir(&root.path).expect("chdir temp");

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
    let paged = root
        .path
        .join(daily_marker_path(CROSSVERIFY_PAGED_MARKER_TASK, today));
    let recorded = root
        .path
        .join(daily_marker_path(CROSSVERIFY_MARKER_TASK, today));

    let handle = spawn_dhan_live_crossverify(deps, &main_feed);
    let real_start = std::time::Instant::now();
    while real_start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_secs(600)).await;
        if paged.exists() {
            break;
        }
    }
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");

    let paged_exists = paged.exists();
    let recorded_exists = recorded.exists();
    let requests = srv
        .seen
        .lock()
        .map(|s| s.request_lines.clone())
        .unwrap_or_default();
    std::env::set_current_dir(&original_cwd).expect("chdir back");
    root.cleanup();

    assert!(
        !recorded_exists,
        "with no token nothing is compared, so the day is never recorded"
    );
    assert!(
        !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
        "the vendor is never called without a token: {requests:?}"
    );
    let (end_day, _) = ist_now();
    if end_day != today {
        return; // crossed IST midnight during the test
    }
    if is_weekday(today) {
        assert!(paged_exists, "a failed weekday pages once and notes it");
    } else {
        assert!(!paged_exists, "a weekend is skipped, nothing is paged");
    }
}
