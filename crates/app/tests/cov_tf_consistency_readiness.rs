//! Coverage tests for `tf_consistency_readiness::wait_for_sealed_candles`:
//! every early-return arm and the deadline-already-passed arm, driven through
//! the process-global `CATCHUP_PROGRESS` handle (this test binary is the only
//! writer, and the tests are serialized by a lock).
//!
//! No network: every arm exercised here returns before the QuestDB read
//! (the seal-writer drain stamp is never set in this process, so the first
//! barrier that can pass is the sealed floor, and the drain barrier fails).

use chrono::NaiveDate;
use tickvault_app::dhan_feed_stack::CATCHUP_PROGRESS;
use tickvault_app::tf_consistency_readiness::{
    TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST, TfNotReady, TfReadiness, wait_for_sealed_candles,
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Today's IST date, computed the way the function computes it.
fn today_ist() -> NaiveDate {
    (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive()
}

/// An arbitrary "day start" (IST wall clock as epoch seconds) far in the
/// past, so the wait deadline derived from it has already passed.
const DAY_START_FOLD_SECS: i64 = 1_000_000;

fn day_start_nanos() -> i64 {
    DAY_START_FOLD_SECS * 1_000_000_000
}

fn init_tracing() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .try_init();
    });
}

async fn wait(date: NaiveDate, day_start: i64) -> TfReadiness {
    init_tracing();
    let client = reqwest::Client::new();
    // Port 1: never contacted on any arm these tests take.
    wait_for_sealed_candles(&client, "http://127.0.0.1:1/exec", date, day_start).await
}

/// Runs the wait for "today" and makes the date the test meant agree with
/// the date the function reads: the two read the clock separately, so a
/// call that straddles IST midnight is simply run again (midnight cannot be
/// crossed twice inside one retry).
async fn wait_today(day_start: i64) -> TfReadiness {
    loop {
        let before = today_ist();
        let r = wait(before, day_start).await;
        if today_ist() == before {
            return r;
        }
    }
}

/// Publishes a floor `sealed` seconds into the target day, runs the wait
/// for today, and clears the progress again.
async fn wait_with_floor_at(sealed: i64) -> TfReadiness {
    let floor = u32::try_from(DAY_START_FOLD_SECS + sealed).expect("fits");
    CATCHUP_PROGRESS.publish(floor, 1_000);
    let r = wait_today(day_start_nanos()).await;
    CATCHUP_PROGRESS.publish(0, 0);
    r
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_past_date_returns_no_floor() {
    let _g = SERIAL.lock().await;
    let past = NaiveDate::from_ymd_opt(2025, 6, 2).expect("date");
    let r = wait(past, day_start_nanos()).await;
    assert_eq!(
        r,
        TfReadiness {
            sealed_through_secs_of_day: None,
            verdict: Ok(()),
        }
    );
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_today_without_progress_returns_no_floor() {
    let _g = SERIAL.lock().await;
    // (0, 0) packs to 0, which reads as "no sweep finished".
    CATCHUP_PROGRESS.publish(0, 0);
    assert_eq!(CATCHUP_PROGRESS.load(), None);
    let r = wait_today(day_start_nanos()).await;
    assert_eq!(r.sealed_through_secs_of_day, None);
    assert_eq!(r.verdict, Ok(()));
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_progress_for_another_day_returns_no_floor() {
    let _g = SERIAL.lock().await;
    // Floor two days past the day start: not inside the target day.
    let floor = u32::try_from(DAY_START_FOLD_SECS + 2 * 86_400).expect("fits");
    CATCHUP_PROGRESS.publish(floor, 1_000);
    let r = wait_today(day_start_nanos()).await;
    CATCHUP_PROGRESS.publish(0, 0);
    assert_eq!(r.sealed_through_secs_of_day, None);
    assert_eq!(r.verdict, Ok(()));
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_sealed_past_target_but_not_drained_reports_pending() {
    let _g = SERIAL.lock().await;
    let sealed = TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST + 300;
    let floor = u32::try_from(DAY_START_FOLD_SECS + sealed).expect("fits");
    CATCHUP_PROGRESS.publish(floor, 1_000);
    let r = wait_today(day_start_nanos()).await;
    CATCHUP_PROGRESS.publish(0, 0);
    // The deadline is long past, so the first sample is final.
    assert_eq!(r.sealed_through_secs_of_day, Some(sealed));
    assert_eq!(r.verdict, Err(TfNotReady::SealsPending));
    assert_eq!(TfNotReady::SealsPending.as_str(), "seals_pending");
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_floor_below_target_reports_not_sealed() {
    let _g = SERIAL.lock().await;
    let sealed = TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST - 60;
    let floor = u32::try_from(DAY_START_FOLD_SECS + sealed).expect("fits");
    CATCHUP_PROGRESS.publish(floor, 1_000);
    let r = wait_today(day_start_nanos()).await;
    CATCHUP_PROGRESS.publish(0, 0);
    assert_eq!(r.sealed_through_secs_of_day, Some(sealed));
    assert_eq!(r.verdict, Err(TfNotReady::NotSealed));
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_floor_at_day_start_is_sod_zero() {
    let _g = SERIAL.lock().await;
    // Boundary: a floor exactly at the day start is second 0 of the day.
    let floor = u32::try_from(DAY_START_FOLD_SECS).expect("fits");
    CATCHUP_PROGRESS.publish(floor, 1);
    let r = wait_today(day_start_nanos()).await;
    CATCHUP_PROGRESS.publish(0, 0);
    assert_eq!(r.sealed_through_secs_of_day, Some(0));
    assert_eq!(r.verdict, Err(TfNotReady::NotSealed));
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_floor_exactly_at_target_passes_sealed_barrier() {
    let _g = SERIAL.lock().await;
    // `sealed < target` is NotSealed, so a floor AT the target is sealed;
    // the drain stamp is never set in this process, so the next barrier
    // (drained after the sweep) is the one reported.
    let r = wait_with_floor_at(TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST).await;
    assert_eq!(
        r.sealed_through_secs_of_day,
        Some(TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST)
    );
    assert_eq!(r.verdict, Err(TfNotReady::SealsPending));
    // One second earlier is still not sealed.
    let r = wait_with_floor_at(TF_VERIFY_SEALED_TARGET_SECS_OF_DAY_IST - 1).await;
    assert_eq!(r.verdict, Err(TfNotReady::NotSealed));
}

#[tokio::test]
async fn test_tf_consistency_readiness_wait_floor_at_last_second_of_day_is_inside_the_day() {
    let _g = SERIAL.lock().await;
    // Second 86,399 is the last second of the target day: a floor there is
    // this day's (past the target, so sealed; then the drain barrier).
    let r = wait_with_floor_at(86_399).await;
    assert_eq!(r.sealed_through_secs_of_day, Some(86_399));
    assert_eq!(r.verdict, Err(TfNotReady::SealsPending));
    // Second 86,400 is the next day: no floor for the target day.
    let r = wait_with_floor_at(86_400).await;
    assert_eq!(r.sealed_through_secs_of_day, None);
    assert_eq!(r.verdict, Ok(()));
}
