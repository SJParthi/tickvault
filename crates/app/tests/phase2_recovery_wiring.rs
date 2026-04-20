//! Integration tests for Phase 2 crash-recovery wiring (PROMPT C, 2026-04-20).
//!
//! Exercises the full boot-side plumbing end-to-end at the pure-function
//! layer:
//!
//! 1. write (or don't write) a snapshot to a tempdir via the public
//!    [`tickvault_storage::phase2_subscription_marker::write_snapshot`],
//! 2. read it back via `read_snapshot`,
//! 3. hand the result + synthetic `(today, now_sec, is_trading_day)`
//!    inputs to [`tickvault_app::phase2_recovery::plan_recovery`],
//! 4. assert the returned [`RecoveryAction`] variant AND the Prometheus
//!    outcome label are both correct.
//!
//! One test per [`RecoveryDecision`] branch (the 4 required per the
//! PROMPT C spec), plus extra ratchets for the snapshot-dispatch payload
//! shape and the label wire-format constants.
//!
//! Metric emission itself is a single-site call in `main.rs`; the label
//! string is asserted here so any accidental rename of the four outcome
//! constants will break this test before the PR can land.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::NaiveDate;

use tickvault_app::phase2_recovery::{
    OUTCOME_FRESH_IMMEDIATE, OUTCOME_REUSED_SNAPSHOT, OUTCOME_SKIP_OFF_HOURS,
    OUTCOME_WAIT_SCHEDULER, PHASE2_SNAPSHOT_PATH, RECOVERY_METRIC_NAME, RecoveryAction,
    plan_recovery,
};
use tickvault_storage::phase2_subscription_marker::{
    PersistedSubscription, Phase2SubscriptionSnapshot, read_snapshot, write_snapshot,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Synthetic "today" — Monday 2026-04-20, matches the PROMPT C directive date.
fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 4, 20).expect("valid")
}

fn yesterday() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 4, 19).expect("valid")
}

/// Seconds-of-day for an `hh:mm` IST wall-clock time.
fn hm(hours: u32, mins: u32) -> u32 {
    hours * 3600 + mins * 60
}

/// Produces a unique tempdir path so parallel test invocations never
/// collide on the same snapshot file.
fn tempdir_path(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("tv-phase2-recovery-{tag}-{pid}-{nanos}"))
}

/// A representative snapshot: two NSE_FNO instruments, one stock with a
/// pre-open reference price. Same shape the real writer produces.
fn sample_snapshot(date: NaiveDate) -> Phase2SubscriptionSnapshot {
    let mut prices = BTreeMap::new();
    prices.insert("RELIANCE".to_string(), 2847.5);
    Phase2SubscriptionSnapshot {
        snapshot_date: date,
        instrument_count: 2,
        reference_prices: prices,
        instruments: vec![
            PersistedSubscription {
                exchange_segment: "NSE_FNO".to_string(),
                security_id: "45001".to_string(),
            },
            PersistedSubscription {
                exchange_segment: "NSE_FNO".to_string(),
                security_id: "45002".to_string(),
            },
        ],
    }
}

/// Builds a snapshot on disk at a unique tempdir path and returns that
/// path. Callers `read_snapshot()` it back to exercise the full round
/// trip.
fn write_tempdir_snapshot(tag: &str, snap: &Phase2SubscriptionSnapshot) -> PathBuf {
    let base = tempdir_path(tag);
    let path = base.join("phase2-subscription.json");
    write_snapshot(&path, snap).expect("write_snapshot must succeed on tempdir");
    path
}

fn cleanup(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

// ---------------------------------------------------------------------------
// The 4 required variants — one test each
// ---------------------------------------------------------------------------

#[test]
fn use_snapshot_branch_rehydrates_instruments_and_labels_reused_snapshot() {
    let snap = sample_snapshot(today());
    let path = write_tempdir_snapshot("use", &snap);
    let loaded = read_snapshot(&path)
        .expect("snapshot file must read back cleanly")
        .expect("snapshot must be present after write");

    // 11:00 IST, trading day — the canonical crash-recovery window.
    let action = plan_recovery(today(), hm(11, 0), true, Some(&loaded));

    match &action {
        RecoveryAction::DispatchSnapshot {
            snapshot_date,
            instrument_count,
            instruments,
        } => {
            assert_eq!(*snapshot_date, today(), "must preserve snapshot_date");
            assert_eq!(*instrument_count, 2);
            assert_eq!(instruments.len(), 2);
            assert_eq!(instruments[0].exchange_segment, "NSE_FNO");
            assert_eq!(instruments[0].security_id, "45001");
            assert_eq!(instruments[1].security_id, "45002");
        }
        other => panic!("expected DispatchSnapshot, got {other:?}"),
    }
    assert_eq!(action.outcome_label(), OUTCOME_REUSED_SNAPSHOT);
    assert!(
        !action.should_spawn_scheduler(),
        "DispatchSnapshot must suppress the scheduler"
    );

    cleanup(&path);
}

#[test]
fn run_fresh_phase2_branch_when_mid_market_with_no_snapshot() {
    let missing = tempdir_path("run-fresh").join("absent.json");
    let loaded = read_snapshot(&missing).expect("absent file must return Ok(None)");
    assert!(loaded.is_none(), "absent file must be None");

    // 11:00 IST, trading day, no snapshot — scheduler's RunImmediate arm.
    let action = plan_recovery(today(), hm(11, 0), true, loaded.as_ref());

    assert!(matches!(action, RecoveryAction::RunFreshPhase2));
    assert_eq!(action.outcome_label(), OUTCOME_FRESH_IMMEDIATE);
    assert!(
        action.should_spawn_scheduler(),
        "RunFreshPhase2 must hand off to the scheduler"
    );
}

#[test]
fn wait_for_scheduler_branch_when_pre_open_with_no_snapshot() {
    // 08:30 IST, trading day — the 09:12 scheduler will fire later.
    let action = plan_recovery(today(), hm(8, 30), true, None);

    assert!(matches!(action, RecoveryAction::WaitForScheduler));
    assert_eq!(action.outcome_label(), OUTCOME_WAIT_SCHEDULER);
    assert!(
        action.should_spawn_scheduler(),
        "WaitForScheduler must let the scheduler sleep until 09:12"
    );
}

#[test]
fn skip_off_hours_branch_when_non_trading_day_with_no_snapshot() {
    // Saturday (non-trading day), mid-day — nothing to subscribe, no snapshot.
    let action = plan_recovery(today(), hm(11, 0), false, None);

    assert!(matches!(action, RecoveryAction::SkipOffHours));
    assert_eq!(action.outcome_label(), OUTCOME_SKIP_OFF_HOURS);
    assert!(
        !action.should_spawn_scheduler(),
        "SkipOffHours must NOT spawn the scheduler"
    );
}

// ---------------------------------------------------------------------------
// Edge cases + ratchets
// ---------------------------------------------------------------------------

#[test]
fn stale_snapshot_mid_market_still_runs_fresh_phase2() {
    // Snapshot from yesterday, mid-market today — the scheduler runs
    // the picker fresh instead of resurrecting yesterday's ATM.
    let stale = sample_snapshot(yesterday());
    let path = write_tempdir_snapshot("stale", &stale);
    let loaded = read_snapshot(&path)
        .expect("snapshot file must read back cleanly")
        .expect("snapshot must be present after write");

    let action = plan_recovery(today(), hm(11, 0), true, Some(&loaded));
    assert!(matches!(action, RecoveryAction::RunFreshPhase2));
    assert_eq!(action.outcome_label(), OUTCOME_FRESH_IMMEDIATE);

    cleanup(&path);
}

#[test]
fn today_snapshot_on_non_trading_day_still_reused() {
    // Operator explicitly asked: a crash recovery at a non-trading-day
    // boot with a fresh today-dated snapshot must still resume — the
    // decision is based on the snapshot's date, not the calendar.
    let snap = sample_snapshot(today());
    let path = write_tempdir_snapshot("today-non-trading", &snap);
    let loaded = read_snapshot(&path)
        .expect("snapshot file must read back cleanly")
        .expect("snapshot must be present after write");

    let action = plan_recovery(today(), hm(11, 0), false, Some(&loaded));
    assert!(matches!(action, RecoveryAction::DispatchSnapshot { .. }));
    assert_eq!(action.outcome_label(), OUTCOME_REUSED_SNAPSHOT);

    cleanup(&path);
}

#[test]
fn post_close_stale_snapshot_skips_off_hours() {
    // 16:00 IST, trading day, snapshot from yesterday — no valid
    // snapshot, market is closed, scheduler would SkipToday(PostMarket).
    let stale = sample_snapshot(yesterday());
    let path = write_tempdir_snapshot("post-close", &stale);
    let loaded = read_snapshot(&path)
        .expect("snapshot file must read back cleanly")
        .expect("snapshot must be present after write");

    let action = plan_recovery(today(), hm(16, 0), true, Some(&loaded));
    assert!(matches!(action, RecoveryAction::SkipOffHours));
    assert_eq!(action.outcome_label(), OUTCOME_SKIP_OFF_HOURS);

    cleanup(&path);
}

#[test]
fn metric_and_path_wire_format_constants_are_pinned() {
    // Ratchet — if any of these constants drift, Prometheus queries,
    // Grafana panels and the PROMPT A writer path will all break.
    assert_eq!(PHASE2_SNAPSHOT_PATH, "data/state/phase2-subscription.json");
    assert_eq!(RECOVERY_METRIC_NAME, "tv_phase2_recovery_total");
    assert_eq!(OUTCOME_REUSED_SNAPSHOT, "reused_snapshot");
    assert_eq!(OUTCOME_FRESH_IMMEDIATE, "fresh_immediate");
    assert_eq!(OUTCOME_WAIT_SCHEDULER, "wait_scheduler");
    assert_eq!(OUTCOME_SKIP_OFF_HOURS, "skip_off_hours");
}

#[test]
fn outcome_labels_are_distinct() {
    // Guard against accidental duplication that would collapse two
    // separate boot scenarios into the same Prometheus series.
    let labels = [
        OUTCOME_REUSED_SNAPSHOT,
        OUTCOME_FRESH_IMMEDIATE,
        OUTCOME_WAIT_SCHEDULER,
        OUTCOME_SKIP_OFF_HOURS,
    ];
    for i in 0..labels.len() {
        for j in (i + 1)..labels.len() {
            assert_ne!(labels[i], labels[j], "labels {i} and {j} collide");
        }
    }
}
