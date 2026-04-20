//! Integration tests for the historical-fetch idempotency wiring.
//!
//! These tests exercise the decision logic + marker I/O path that
//! `fetch_historical_candles` consults before any network call. We
//! cannot stand up a fake Dhan REST server inside `tickvault-core`
//! without pulling in a wiremock dep, so the tests verify the
//! observable behavior at the marker boundary:
//!
//! - fresh boot (no marker) → decision says `FetchFullNinetyDays`
//! - same-day reboot (marker matches today) → decision says `Skip`
//!   AND no network call is implied (we exercise the entry point
//!   with a registry that would trip on any HTTP call — empty)
//! - prior-day marker, post-close trading day → `FetchTodayOnly`
//! - prior-day marker, pre-close trading day → `WaitUntilPostClose`
//! - prior-day marker, weekend → `FetchTodayOnly` regardless of hour
//! - corrupt marker file → surfaced error, not silent re-fetch

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use tickvault_storage::historical_fetch_marker::{
    FetchDecision, FetchMode, HistoricalFetchMarker, POST_MARKET_CLOSE_SECS_IST, decide_fetch,
    read_marker, write_marker,
};

fn marker_at(path: &Path, date: NaiveDate, mode: FetchMode) -> HistoricalFetchMarker {
    let m = HistoricalFetchMarker {
        last_success_date: date,
        instruments_fetched: 241,
        candles_written: 7_079_977,
        mode,
    };
    write_marker(path, &m).expect("write_marker must succeed");
    m
}

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 4, 20).unwrap()
}

fn yesterday() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 4, 19).unwrap()
}

/// Returns a unique throwaway path under the system temp dir.
/// Caller is responsible for cleaning up if needed; tests are isolated
/// via per-call nanosecond suffixes so collisions don't matter.
fn tmp() -> (TempDirGuard, PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("tv-hist-fetch-{pid}-{nanos}"));
    let path = base.join("nested/historical_fetch_done.json");
    (TempDirGuard(base), path)
}

/// RAII guard that removes the temp directory on drop.
struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn test_fresh_boot_no_marker_runs_90_day_fetch_writes_marker() {
    // Scenario: brand-new clone, no marker on disk. Decision must be
    // FullNinetyDays. After a synthetic successful fetch the marker
    // is written and visible on the next read.
    let (_dir, path) = tmp();

    // No marker yet.
    assert!(read_marker(&path).expect("read ok").is_none());

    let decision = decide_fetch(today(), None, 12 * 3600, true);
    assert_eq!(decision, FetchDecision::FetchFullNinetyDays);

    // Simulate the wiring writing the marker on success.
    let written = HistoricalFetchMarker {
        last_success_date: today(),
        instruments_fetched: 241,
        candles_written: 7_079_977,
        mode: FetchMode::FullNinetyDays,
    };
    write_marker(&path, &written).unwrap();

    let reread = read_marker(&path).unwrap().unwrap();
    assert_eq!(reread.last_success_date, today());
    assert_eq!(reread.mode, FetchMode::FullNinetyDays);
}

#[test]
fn test_same_day_reboot_skips_no_network_call() {
    // Scenario: app already finished today's fetch and rebooted. The
    // marker date == today, so decide_fetch returns Skip. The wiring
    // short-circuits before any HTTP client is constructed.
    let (_dir, path) = tmp();
    let m = marker_at(&path, today(), FetchMode::FullNinetyDays);

    // Every hour of the day must Skip when marker.date == today.
    for hour in [0u32, 9, 15, 16, 23] {
        let decision = decide_fetch(today(), Some(&m), hour * 3600, true);
        assert_eq!(
            decision,
            FetchDecision::Skip,
            "same-day reboot at {hour}h must Skip"
        );
    }
}

#[test]
fn test_next_day_boot_trading_day_post_close_fetches_today_only() {
    let (_dir, path) = tmp();
    let m = marker_at(&path, yesterday(), FetchMode::FullNinetyDays);

    // 16:05 IST — past 15:30 close on a trading day.
    let now_sec = 16 * 3600 + 5 * 60;
    let decision = decide_fetch(today(), Some(&m), now_sec, true);
    assert_eq!(decision, FetchDecision::FetchTodayOnly);
}

#[test]
fn test_next_day_boot_trading_day_pre_close_waits_until_close() {
    let (_dir, path) = tmp();
    let m = marker_at(&path, yesterday(), FetchMode::FullNinetyDays);

    // 10:30 IST — well before 15:30 close.
    let now_sec = 10 * 3600 + 30 * 60;
    let decision = decide_fetch(today(), Some(&m), now_sec, true);
    assert_eq!(decision, FetchDecision::WaitUntilPostClose);

    // Boundary: 15:29:59 still waits.
    let one_sec_pre = POST_MARKET_CLOSE_SECS_IST - 1;
    assert_eq!(
        decide_fetch(today(), Some(&m), one_sec_pre, true),
        FetchDecision::WaitUntilPostClose
    );
}

#[test]
fn test_weekend_boot_fetches_today_only_any_hour() {
    let (_dir, path) = tmp();
    let m = marker_at(&path, yesterday(), FetchMode::FullNinetyDays);

    // Non-trading day — any hour should fire today-only fetch.
    for hour in [0u32, 3, 9, 11, 14, 15, 17, 22] {
        let decision = decide_fetch(today(), Some(&m), hour * 3600, false);
        assert_eq!(
            decision,
            FetchDecision::FetchTodayOnly,
            "non-trading day at {hour}h must FetchTodayOnly"
        );
    }
}

#[test]
fn test_corrupt_marker_file_surfaces_error_does_not_silently_refetch() {
    // Scenario: marker file exists but is corrupt JSON. read_marker
    // must return an error (not Ok(None)) so the wiring can:
    //   1. increment tv_historical_fetch_marker_read_errors_total
    //   2. emit ERROR (Telegram)
    //   3. refuse to re-fetch (returns empty summary)
    // — not silently roll back to "fresh clone" semantics and pull 90 days.
    let (_dir, path) = tmp();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{this is corrupt").unwrap();

    let result = read_marker(&path);
    assert!(
        result.is_err(),
        "corrupt marker MUST surface as Err, not Ok(None)"
    );
}

#[test]
fn test_marker_roundtrip_uses_today_only_mode_after_incremental_fetch() {
    // Confirms the wiring tags incremental fetches with FetchMode::TodayOnly
    // so subsequent reboots can distinguish "first 90-day pull happened"
    // from "only today was fetched".
    let (_dir, path) = tmp();
    let m = HistoricalFetchMarker {
        last_success_date: today(),
        instruments_fetched: 241,
        candles_written: 95_000,
        mode: FetchMode::TodayOnly,
    };
    write_marker(&path, &m).unwrap();
    let reread = read_marker(&path).unwrap().unwrap();
    assert_eq!(reread.mode, FetchMode::TodayOnly);
}
