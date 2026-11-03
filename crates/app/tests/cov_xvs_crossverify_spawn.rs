//! Coverage for `dhan_live_crossverify_boot::spawn_dhan_live_crossverify` and
//! `live_side_readiness`, against a QuestDB-shaped server on 127.0.0.1 and a
//! paused tokio clock.
//!
//! Tokio's paused clock does not advance chrono wall time. The public-spawn
//! fixture closes the calendar across the current/catch-up dates and verifies
//! arming, no page/vendor call/success marker, and cancellation at any real
//! time of day. The private fixed-time unit fixture exercises the real missing-
//! token attempt, once-per-day page claim and durable marker separately.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use cov_xvs_support::{
    CwdGuard, FakeQuestDb, captured_events, drive_paused, init_no_proxy, init_tracing, is_weekday,
    ist_now, lock_cwd,
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

fn calendar_closed_around(
    today: chrono::NaiveDate,
) -> Arc<tickvault_common::trading_calendar::TradingCalendar> {
    use tickvault_common::config::{NseHolidayEntry, TradingConfig};
    let mut holidays = Vec::new();
    for offset in -tickvault_storage::partition_archive::MAX_CROSSVERIFY_HOLD_DAYS..=1 {
        let date = today
            .checked_add_signed(chrono::TimeDelta::days(offset))
            .expect("fixture date window");
        if is_weekday(date) {
            holidays.push(NseHolidayEntry {
                date: date.to_string(),
                name: "Crossverify fixture closed session".to_owned(),
            });
        }
    }
    Arc::new(
        tickvault_common::trading_calendar::TradingCalendar::from_config(&TradingConfig {
            market_open_time: "09:15:00".to_owned(),
            market_close_time: "15:30:00".to_owned(),
            order_cutoff_time: "15:29:00".to_owned(),
            data_collection_start: "09:00:00".to_owned(),
            data_collection_end: "16:00:00".to_owned(),
            timezone: "Asia/Kolkata".to_owned(),
            max_orders_per_second: 10,
            nse_holidays: holidays,
            muhurat_trading_dates: Vec::new(),
            nse_mock_trading_dates: Vec::new(),
        })
        .expect("closed fixture calendar"),
    )
}

#[tokio::test(start_paused = true)]
async fn test_dhan_live_crossverify_boot_spawn_closed_calendar_never_pages_and_can_be_cancelled() {
    init_no_proxy();
    init_tracing();
    let _cwd_lock = lock_cwd().await;
    let cwd = CwdGuard::enter("crossverify-spawn");
    let pages_before = page_lines();
    let armed_before = armed_lines();

    let srv = FakeQuestDb::start_default();
    let (today, _) = ist_now();
    let calendar = calendar_closed_around(today);
    assert!(!calendar.is_trading_day(today));
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
        calendar,
        // This fixture configures a static feed; no missing master-list
        // fallback or successful master discovery is being simulated.
        universe_basis: tickvault_app::xverify_day_record::UniverseBasis::ConfiguredIndices,
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
    let paged = cwd
        .path()
        .join(daily_marker_path(CROSSVERIFY_PAGED_MARKER_TASK, today));
    let recorded = cwd
        .path()
        .join(daily_marker_path(CROSSVERIFY_MARKER_TASK, today));

    let handle = spawn_dhan_live_crossverify(deps, &main_feed);
    // Exercise timer wakeups without pretending they advance chrono's wall
    // clock. The explicitly closed calendar makes paging invalid both before
    // and after 15:41, including the bounded past-day catch-up.
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
        "a closed fixture day is never compared or recorded"
    );
    assert!(
        !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
        "the vendor is never called without a token: {requests:?}"
    );
    assert!(!paged_seen, "an explicitly closed fixture day never pages");
    assert_eq!(
        pages, 0,
        "closed current and catch-up days produce no failure page"
    );
}
