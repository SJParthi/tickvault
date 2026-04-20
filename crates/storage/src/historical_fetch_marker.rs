//! Historical-fetch idempotency marker (Parthiban spec, 2026-04-20).
//!
//! # Why this module exists
//!
//! Before today, every app restart re-pulled the full 90 days of
//! historical candles from Dhan — even multiple times per day — which:
//! - wasted Dhan's daily-data quota,
//! - burned tens of minutes of boot time,
//! - triggered DH-904 rate-limit alert storms, and
//! - produced no new data (QuestDB DEDUP just ignored the overlap).
//!
//! This module enforces Parthiban's spec (verbatim, 2026-04-20):
//!
//! > "when it is working trading day it should always fetch the
//! >  historical candles only for the very first time alone 90 days,
//! >  if not first time then only current needed working day alone,
//! >  that too only after post market close right I mean after 3.30 pm
//! >  alone only right. Suppose if it is non working or non trading
//! >  day then it is completely fine to pull the data at any point of
//! >  time irrespective of hours. See that whatever the situation it
//! >  is it should always fetch only once in a day — I mean successful
//! >  fetch — considering all these cases scenarios".
//!
//! # Decision matrix (MECE)
//!
//! | Marker state (today's successful fetch) | Trading day? | Now < 15:30 IST? | Decision             |
//! |------------------------------------------|--------------|-------------------|----------------------|
//! | Already succeeded today                  | *            | *                 | `Skip`               |
//! | No prior success (fresh clone)           | *            | *                 | `FetchFullNinetyDays`|
//! | Prior success ≥ 1 day ago                | yes          | yes (pre-close)   | `WaitUntilPostClose` |
//! | Prior success ≥ 1 day ago                | yes          | no  (post-close)  | `FetchTodayOnly`     |
//! | Prior success ≥ 1 day ago                | no           | *                 | `FetchTodayOnly`     |
//!
//! "Trading day" is the caller's responsibility (must consult
//! `TradingCalendar::is_trading_day(today)`). This module is
//! deliberately pure + allocation-free on the hot path so it can be
//! called from the boot sequence without pulling in calendar deps.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// The idempotency marker persisted to disk.
///
/// Shape is a single-object JSON file so it is trivially diffable
/// and human-inspectable. Example on disk:
///
/// ```json
/// {
///   "last_success_date": "2026-04-20",
///   "instruments_fetched": 241,
///   "candles_written": 7079977,
///   "mode": "FullNinetyDays"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalFetchMarker {
    /// IST date of the LAST successful fetch. If absent, no fetch
    /// has ever succeeded on this machine.
    pub last_success_date: NaiveDate,
    /// Count of instruments successfully fetched in the last run.
    /// Purely diagnostic — not used by the decision logic.
    pub instruments_fetched: u32,
    /// Total candles written across all timeframes — diagnostic.
    pub candles_written: u64,
    /// Which branch of the decision tree produced this marker.
    pub mode: FetchMode,
}

/// Which branch of the decision tree ran. Matches [`FetchDecision`]
/// variants that actually fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchMode {
    /// First-ever fetch on this machine or cache wipe — pulled 90 days.
    FullNinetyDays,
    /// Incremental fetch — pulled only today's new candle(s).
    TodayOnly,
}

/// Decision returned by [`decide_fetch`] — the ONLY hot-path entry.
///
/// The caller dispatches based on this; this module does not do
/// any network or disk I/O except via the separate
/// [`read_marker`] / [`write_marker`] helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchDecision {
    /// `last_success_date == today` → nothing to do.
    Skip,
    /// No marker on disk — pull the full 90-day window.
    FetchFullNinetyDays,
    /// Prior fetch succeeded on an earlier date AND we are post-15:30
    /// IST on a trading day (or it is a non-trading day). Pull only
    /// today's data.
    FetchTodayOnly,
    /// Prior fetch succeeded on an earlier date, today IS a trading
    /// day, and the clock is before 15:30 IST. Caller should schedule
    /// the fetch for post-close instead of firing immediately.
    WaitUntilPostClose,
}

/// Post-market close, in seconds since IST midnight (15:30 IST).
/// Kept as a pub const so tests can reference the exact boundary.
pub const POST_MARKET_CLOSE_SECS_IST: u32 = 15 * 3600 + 30 * 60;

/// Core decision function — PURE, no I/O, no allocations on the hot
/// path (returns an enum). Takes the marker by reference so callers
/// can keep their owned copy.
///
/// # Arguments
/// - `today`: IST calendar date the boot is running on.
/// - `marker`: previous successful fetch record, or `None` on first
///   boot.
/// - `now_sec_of_day_ist`: current wall-clock seconds since IST
///   midnight (0..86_400).
/// - `is_trading_day`: caller-supplied — typically
///   `TradingCalendar::is_trading_day(today)`.
///
/// # Performance
///
/// O(1) — at most one `NaiveDate` equality compare, one u32 compare,
/// one branch. Safe to call from boot hot path without measurement.
#[must_use]
pub fn decide_fetch(
    today: NaiveDate,
    marker: Option<&HistoricalFetchMarker>,
    now_sec_of_day_ist: u32,
    is_trading_day: bool,
) -> FetchDecision {
    // Case A: already succeeded today → skip.
    if let Some(m) = marker
        && m.last_success_date == today
    {
        return FetchDecision::Skip;
    }

    // Case B: no marker on disk → first boot ever. Pull 90 days now
    // regardless of hour or trading-day status. The first-boot case
    // is the one and only time we pull 90 days; see `FetchMode` docs.
    let Some(_prior) = marker else {
        return FetchDecision::FetchFullNinetyDays;
    };

    // Case C: prior success was on an earlier date.
    //   On a non-trading day we can fetch anytime (market is closed,
    //   no rate-sensitive load). On a trading day we must wait until
    //   post-15:30 IST to avoid stepping on the live feed.
    if !is_trading_day {
        return FetchDecision::FetchTodayOnly;
    }
    if now_sec_of_day_ist < POST_MARKET_CLOSE_SECS_IST {
        FetchDecision::WaitUntilPostClose
    } else {
        FetchDecision::FetchTodayOnly
    }
}

/// Reads the marker file if it exists. Returns `None` when the file
/// is absent (fresh clone). Returns an error only for genuine I/O
/// or parse failures — a corrupt marker is NOT silently ignored so
/// the caller can alert the operator.
pub fn read_marker(path: &Path) -> Result<Option<HistoricalFetchMarker>, MarkerIoError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| MarkerIoError::Read {
        path: path.to_path_buf(),
        source: e,
    })?;
    let parsed: HistoricalFetchMarker =
        serde_json::from_slice(&bytes).map_err(|e| MarkerIoError::Parse {
            path: path.to_path_buf(),
            source: e,
        })?;
    Ok(Some(parsed))
}

/// Writes the marker file atomically via temp-file + rename so a
/// crash mid-write never leaves a torn file.
pub fn write_marker(path: &Path, marker: &HistoricalFetchMarker) -> Result<(), MarkerIoError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| MarkerIoError::Write {
            path: path.to_path_buf(),
            source: e,
        })?;
    }
    let json = serde_json::to_vec_pretty(marker).map_err(|e| MarkerIoError::Serialize {
        path: path.to_path_buf(),
        source: e,
    })?;
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json).map_err(|e| MarkerIoError::Write {
        path: tmp_path.clone(),
        source: e,
    })?;
    fs::rename(&tmp_path, path).map_err(|e| MarkerIoError::Rename {
        from: tmp_path,
        to: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Errors from [`read_marker`] / [`write_marker`]. Always include
/// the path so operators can locate the file.
#[derive(Debug, thiserror::Error)]
pub enum MarkerIoError {
    #[error("failed to read historical-fetch marker at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse historical-fetch marker at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize historical-fetch marker for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write historical-fetch marker at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to atomically rename {from} -> {to}: {source}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 4, 20).unwrap()
    }

    fn yesterday() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 4, 19).unwrap()
    }

    fn marker_for(date: NaiveDate, mode: FetchMode) -> HistoricalFetchMarker {
        HistoricalFetchMarker {
            last_success_date: date,
            instruments_fetched: 241,
            candles_written: 7_079_977,
            mode,
        }
    }

    #[test]
    fn test_decide_first_boot_ever_returns_full_ninety_days_regardless_of_hour() {
        // No marker → 90-day pull, even at 02:00 IST on a non-trading day.
        for hour in [0, 3, 9, 12, 16, 20, 23] {
            let now = hour * 3600;
            for is_trading in [true, false] {
                let decision = decide_fetch(today(), None, now, is_trading);
                assert_eq!(
                    decision,
                    FetchDecision::FetchFullNinetyDays,
                    "first boot @ {hour}h is_trading={is_trading} must pull 90 days"
                );
            }
        }
    }

    #[test]
    fn test_decide_already_fetched_today_skips() {
        // Marker date == today → always skip regardless of any other input.
        let m = marker_for(today(), FetchMode::FullNinetyDays);
        for hour in [0, 9, 15, 16, 23] {
            for is_trading in [true, false] {
                let decision = decide_fetch(today(), Some(&m), hour * 3600, is_trading);
                assert_eq!(decision, FetchDecision::Skip);
            }
        }
    }

    #[test]
    fn test_decide_prior_fetch_trading_day_pre_close_waits() {
        // Marker is yesterday, today is trading, now = 10:30 IST.
        let m = marker_for(yesterday(), FetchMode::FullNinetyDays);
        let now = 10 * 3600 + 30 * 60; // 10:30 IST
        assert_eq!(
            decide_fetch(today(), Some(&m), now, true),
            FetchDecision::WaitUntilPostClose,
            "pre-close on a trading day must wait, not fetch"
        );
    }

    #[test]
    fn test_decide_prior_fetch_trading_day_exactly_1530_fetches() {
        // At 15:30:00 sharp we are post-close — inclusive lower bound.
        let m = marker_for(yesterday(), FetchMode::FullNinetyDays);
        let now = POST_MARKET_CLOSE_SECS_IST; // 15:30:00 IST
        assert_eq!(
            decide_fetch(today(), Some(&m), now, true),
            FetchDecision::FetchTodayOnly
        );
    }

    #[test]
    fn test_decide_prior_fetch_trading_day_post_close_fetches_today_only() {
        let m = marker_for(yesterday(), FetchMode::FullNinetyDays);
        let now = 16 * 3600 + 5 * 60; // 16:05 IST
        assert_eq!(
            decide_fetch(today(), Some(&m), now, true),
            FetchDecision::FetchTodayOnly
        );
    }

    #[test]
    fn test_decide_prior_fetch_non_trading_day_fetches_anytime() {
        // Weekend / holiday — any hour is acceptable.
        let m = marker_for(yesterday(), FetchMode::FullNinetyDays);
        for hour in [0, 3, 9, 11, 15, 20, 23] {
            let now = hour * 3600;
            assert_eq!(
                decide_fetch(today(), Some(&m), now, false),
                FetchDecision::FetchTodayOnly,
                "non-trading day @ {hour}h must fetch anytime"
            );
        }
    }

    #[test]
    fn test_decide_prior_fetch_just_before_close_still_waits() {
        // 15:29:59 IST — one second before the close boundary.
        let m = marker_for(yesterday(), FetchMode::FullNinetyDays);
        let now = POST_MARKET_CLOSE_SECS_IST - 1;
        assert_eq!(
            decide_fetch(today(), Some(&m), now, true),
            FetchDecision::WaitUntilPostClose
        );
    }

    #[test]
    fn test_decide_prior_fetch_much_older_than_yesterday_still_valid() {
        // If the machine was off for 2 weeks, we still only fetch
        // today's window — the 90-day historical data from last run
        // is already persisted.
        let two_weeks_ago = NaiveDate::from_ymd_opt(2026, 4, 6).unwrap();
        let m = marker_for(two_weeks_ago, FetchMode::FullNinetyDays);
        let now = 17 * 3600; // 17:00 IST, post-close
        assert_eq!(
            decide_fetch(today(), Some(&m), now, true),
            FetchDecision::FetchTodayOnly
        );
    }

    #[test]
    fn test_post_market_close_constant_equals_1530_ist() {
        assert_eq!(POST_MARKET_CLOSE_SECS_IST, 15 * 3600 + 30 * 60);
        assert_eq!(POST_MARKET_CLOSE_SECS_IST, 55_800);
    }

    #[test]
    fn test_read_marker_returns_none_when_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-marker-missing-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let result = read_marker(&tmp).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_write_then_read_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-marker-roundtrip-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let m = marker_for(today(), FetchMode::FullNinetyDays);
        write_marker(&tmp, &m).unwrap();
        let read = read_marker(&tmp).unwrap().unwrap();
        assert_eq!(read, m);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_read_marker_returns_parse_error_on_corrupt_file() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-marker-corrupt-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, b"{this is not valid json}").unwrap();
        let err = read_marker(&tmp).unwrap_err();
        assert!(matches!(err, MarkerIoError::Parse { .. }));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_write_marker_creates_parent_directory() {
        let base = std::env::temp_dir().join(format!(
            "tv-marker-parent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let tmp = base.join("nested/dir/marker.json");
        let m = marker_for(today(), FetchMode::TodayOnly);
        write_marker(&tmp, &m).unwrap();
        assert!(tmp.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_fetch_mode_serde_roundtrip() {
        for mode in [FetchMode::FullNinetyDays, FetchMode::TodayOnly] {
            let s = serde_json::to_string(&mode).unwrap();
            let back: FetchMode = serde_json::from_str(&s).unwrap();
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn test_fetch_decision_variants_are_distinct() {
        use FetchDecision::*;
        let all = [
            Skip,
            FetchFullNinetyDays,
            FetchTodayOnly,
            WaitUntilPostClose,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }
}
