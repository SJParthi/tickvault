//! Crash-recovery marker for Phase 2 stock F&O subscriptions (Parthiban, 2026-04-20).
//!
//! # Why this module exists
//!
//! At 09:12:30 IST the Phase 2 scheduler picks the day's stock F&O
//! subscriptions — typically ~200 stocks × (51 CE + 51 PE + 1 future)
//! ≈ 20,600 `(segment, security_id)` pairs. Those picks are pinned
//! to the 09:12 reference price; if the app crashes or is restarted
//! at (say) 11:00 IST, rebuilding the picks from the live 11:00 LTP
//! would produce a *different* ATM and subscribe a different chain,
//! breaking continuity.
//!
//! Parthiban's spec (2026-04-20, verbatim):
//!
//! > "if it is fresh clone and app itself started only for the very
//! >  first time inside market hours we can follow the same process
//! >  right. But suppose in the worst case due to some issues or any
//! >  concerns app crashed or db crashed or anything crashed ...
//! >  either manually or automatically app gets started or restarted
//! >  means then it should have the reference of earlier subscription
//! >  right and then it should pick it up right".
//!
//! # What this module ships
//!
//! - `Phase2SubscriptionSnapshot` — the serialized record.
//! - Read/write helpers using the same atomic temp+rename pattern as
//!   `historical_fetch_marker`.
//! - `RecoveryDecision` — four-arm decision enum:
//!     - `UseSnapshot { snapshot }` — valid snapshot for today, resume.
//!     - `RunFreshPhase2` — no snapshot or snapshot is from an earlier
//!       day → run the regular Phase 2 picker.
//!     - `WaitForPhase2Scheduler` — boot is BEFORE 09:12 IST on a
//!       trading day → no snapshot needed yet; let the scheduler run.
//!     - `SkipPhase2OutsideMarketHours` — boot is outside market hours
//!       AND no snapshot exists → honor the market-hours gate, do
//!       nothing this boot. (A fresh-clone weekend boot does NOT
//!       pretend to subscribe stock F&O.)
//!
//! All decision functions are PURE — no I/O, no allocations, no locks.
//! Read/write functions do filesystem I/O but never on the hot path.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// A single subscription entry in the persisted snapshot. Mirrors
/// `tickvault_core::websocket::types::InstrumentSubscription` but
/// stays in `storage` so this crate does not depend on `core`
/// (core already depends on storage, so a reverse dependency would
/// introduce a cycle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSubscription {
    /// Exchange segment as the same wire string core emits
    /// (`"NSE_FNO"`, `"BSE_FNO"`, etc.).
    pub exchange_segment: String,
    /// Security ID as a decimal string (matches Dhan JSON convention
    /// and the live serialized form on the wire).
    pub security_id: String,
}

/// The persisted snapshot written once per day at 09:12:30 IST when
/// the Phase 2 picker completes successfully.
///
/// `PartialEq` only (no `Eq`) because `reference_prices` holds `f64`
/// values which cannot be `Eq` (NaN != NaN). `PartialEq` is enough
/// for test assertions; the snapshot is otherwise compared field by
/// field in production paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Phase2SubscriptionSnapshot {
    /// IST date the snapshot was produced.
    pub snapshot_date: NaiveDate,
    /// Total number of `(segment, security_id)` pairs — diagnostic.
    pub instrument_count: usize,
    /// Reference prices per stock symbol used to pick ATM — kept so
    /// recovery logs can explain *why* each subscription was chosen.
    pub reference_prices: std::collections::BTreeMap<String, f64>,
    /// Flat list of subscriptions to re-dispatch on recovery.
    pub instruments: Vec<PersistedSubscription>,
}

/// The decision the boot sequence takes after consulting the snapshot.
///
/// `PartialEq` only because the `UseSnapshot` variant embeds a
/// [`Phase2SubscriptionSnapshot`], which is not `Eq` (see its doc).
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryDecision {
    /// Snapshot on disk was produced today — re-dispatch the same
    /// subscriptions without re-running the ATM picker. This is the
    /// primary crash-recovery path for an 11:00 IST restart.
    UseSnapshot {
        snapshot: Phase2SubscriptionSnapshot,
    },
    /// No snapshot OR snapshot is stale (from an earlier day) AND
    /// we are inside the Phase 2 window (≥ 09:12 IST) on a trading
    /// day — run the normal picker.
    RunFreshPhase2,
    /// Trading day, boot is before 09:12 IST — the regular scheduler
    /// at 09:12 will handle Phase 2, no action required now.
    WaitForPhase2Scheduler,
    /// Outside market hours AND no snapshot → stock F&O is not
    /// subscribed this boot (consistent with audit-findings Rule 3,
    /// all background workers must be market-hours aware).
    SkipPhase2OutsideMarketHours,
}

/// Seconds since IST midnight corresponding to 09:12:00 — the Phase 2
/// scheduler trigger time.
pub const PHASE2_TRIGGER_SECS_IST: u32 = 9 * 3600 + 12 * 60;

/// Seconds since IST midnight corresponding to 15:30:00 IST (close).
pub const MARKET_CLOSE_SECS_IST: u32 = 15 * 3600 + 30 * 60;

/// Pure decision function — no I/O.
///
/// # Arguments
/// - `today`: IST calendar date.
/// - `snapshot`: result of [`read_snapshot`] (or `None` if absent).
/// - `now_sec_of_day_ist`: seconds since IST midnight (0..86_400).
/// - `is_trading_day`: supplied by the caller's `TradingCalendar`.
///
/// # Performance
/// O(1). No allocations, no I/O.
#[must_use]
pub fn decide_recovery(
    today: NaiveDate,
    snapshot: Option<&Phase2SubscriptionSnapshot>,
    now_sec_of_day_ist: u32,
    is_trading_day: bool,
) -> RecoveryDecision {
    let in_market_hours = is_trading_day
        && (PHASE2_TRIGGER_SECS_IST..MARKET_CLOSE_SECS_IST).contains(&now_sec_of_day_ist);
    let pre_trigger_trading_day = is_trading_day && now_sec_of_day_ist < PHASE2_TRIGGER_SECS_IST;

    // Case A — valid today-dated snapshot wins every time. The
    // operator's expectation is that a mid-market restart at 11:00
    // resumes exactly the ATM chain that the 09:12 scheduler picked.
    if let Some(s) = snapshot
        && s.snapshot_date == today
    {
        return RecoveryDecision::UseSnapshot {
            snapshot: s.clone(),
        };
    }

    // Case B — before 09:12 on a trading day → scheduler will handle it.
    // Matches the "fresh clone booted pre-open" scenario.
    if pre_trigger_trading_day {
        return RecoveryDecision::WaitForPhase2Scheduler;
    }

    // Case C — inside the Phase 2 window (09:12..15:30) on a trading
    // day without a valid snapshot → run the regular picker now.
    // Covers fresh-clone-during-market-hours plus stale-snapshot
    // (snapshot from a prior day).
    if in_market_hours {
        return RecoveryDecision::RunFreshPhase2;
    }

    // Case D — outside market hours AND no usable snapshot. Skip.
    // This avoids a weekend/post-close boot subscribing stock F&O
    // based on stale data or empty LTP buffers.
    RecoveryDecision::SkipPhase2OutsideMarketHours
}

/// Reads the snapshot file, returning `None` when the file is missing
/// (fresh clone). Corrupt files surface as `MarkerIoError::Parse` so
/// the operator is alerted rather than silently losing continuity.
pub fn read_snapshot(
    path: &Path,
) -> Result<Option<Phase2SubscriptionSnapshot>, SubscriptionMarkerError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| SubscriptionMarkerError::Read {
        path: path.to_path_buf(),
        source: e,
    })?;
    let parsed: Phase2SubscriptionSnapshot =
        serde_json::from_slice(&bytes).map_err(|e| SubscriptionMarkerError::Parse {
            path: path.to_path_buf(),
            source: e,
        })?;
    Ok(Some(parsed))
}

/// Atomically writes the snapshot via temp-file + rename so a crash
/// mid-write cannot leave a torn JSON file.
pub fn write_snapshot(
    path: &Path,
    snapshot: &Phase2SubscriptionSnapshot,
) -> Result<(), SubscriptionMarkerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| SubscriptionMarkerError::Write {
            path: path.to_path_buf(),
            source: e,
        })?;
    }
    let json =
        serde_json::to_vec_pretty(snapshot).map_err(|e| SubscriptionMarkerError::Serialize {
            path: path.to_path_buf(),
            source: e,
        })?;
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json).map_err(|e| SubscriptionMarkerError::Write {
        path: tmp_path.clone(),
        source: e,
    })?;
    fs::rename(&tmp_path, path).map_err(|e| SubscriptionMarkerError::Rename {
        from: tmp_path,
        to: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Errors surfaced by [`read_snapshot`] / [`write_snapshot`].
#[derive(Debug, thiserror::Error)]
pub enum SubscriptionMarkerError {
    #[error("failed to read Phase 2 subscription snapshot at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Phase 2 subscription snapshot at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize Phase 2 subscription snapshot for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write Phase 2 subscription snapshot at {path}: {source}")]
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

    fn sample_snapshot(date: NaiveDate) -> Phase2SubscriptionSnapshot {
        let mut prices = std::collections::BTreeMap::new();
        prices.insert("RELIANCE".to_string(), 2847.5);
        prices.insert("INFY".to_string(), 1520.0);
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

    #[test]
    fn test_decide_today_snapshot_wins_regardless_of_time_of_day() {
        let snap = sample_snapshot(today());
        for hour in [0, 9, 12, 14, 16, 23] {
            let decision = decide_recovery(today(), Some(&snap), hour * 3600, true);
            match decision {
                RecoveryDecision::UseSnapshot { snapshot } => {
                    assert_eq!(snapshot.snapshot_date, today());
                }
                other => panic!("expected UseSnapshot @ {hour}h, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_decide_today_snapshot_wins_on_non_trading_day_too() {
        // Crash recovery must work even on a weekend-after-trading-Friday
        // restart (though normally the app would not start at all then).
        let snap = sample_snapshot(today());
        let decision = decide_recovery(today(), Some(&snap), 11 * 3600, false);
        assert!(matches!(decision, RecoveryDecision::UseSnapshot { .. }));
    }

    #[test]
    fn test_decide_pre_trigger_trading_day_waits_for_scheduler() {
        // Fresh clone booted at 08:30 IST on a Monday — no snapshot yet,
        // scheduler fires at 09:12.
        let decision = decide_recovery(today(), None, 8 * 3600 + 30 * 60, true);
        assert_eq!(decision, RecoveryDecision::WaitForPhase2Scheduler);
    }

    #[test]
    fn test_decide_pre_trigger_with_stale_snapshot_still_waits() {
        // Stale snapshot from yesterday — but it's 08:30 today and a
        // trading day, so the scheduler will replace it at 09:12.
        let snap = sample_snapshot(yesterday());
        let decision = decide_recovery(today(), Some(&snap), 8 * 3600 + 30 * 60, true);
        assert_eq!(decision, RecoveryDecision::WaitForPhase2Scheduler);
    }

    #[test]
    fn test_decide_mid_market_fresh_clone_runs_phase2() {
        // Fresh clone first-ever boot at 11:00 IST on a trading day.
        // No snapshot → the regular picker runs.
        let decision = decide_recovery(today(), None, 11 * 3600, true);
        assert_eq!(decision, RecoveryDecision::RunFreshPhase2);
    }

    #[test]
    fn test_decide_mid_market_stale_snapshot_runs_phase2() {
        let snap = sample_snapshot(yesterday());
        // 11:00 IST, trading day, stale snapshot → RunFreshPhase2.
        let decision = decide_recovery(today(), Some(&snap), 11 * 3600, true);
        assert_eq!(decision, RecoveryDecision::RunFreshPhase2);
    }

    #[test]
    fn test_decide_exactly_at_0912_uses_run_fresh_when_no_snapshot() {
        // At 09:12:00 sharp with no snapshot, we run (scheduler is firing).
        let decision = decide_recovery(today(), None, PHASE2_TRIGGER_SECS_IST, true);
        assert_eq!(decision, RecoveryDecision::RunFreshPhase2);
    }

    #[test]
    fn test_decide_exactly_at_0911_waits_when_no_snapshot() {
        // 09:11:59 IST, no snapshot → wait for scheduler.
        let decision = decide_recovery(today(), None, PHASE2_TRIGGER_SECS_IST - 1, true);
        assert_eq!(decision, RecoveryDecision::WaitForPhase2Scheduler);
    }

    #[test]
    fn test_decide_exactly_at_1530_skips_when_no_snapshot() {
        // 15:30 is the close boundary; ≥ close → skip.
        let decision = decide_recovery(today(), None, MARKET_CLOSE_SECS_IST, true);
        assert_eq!(decision, RecoveryDecision::SkipPhase2OutsideMarketHours);
    }

    #[test]
    fn test_decide_non_trading_day_no_snapshot_skips() {
        // Weekend restart with no snapshot — no action.
        for hour in [0, 8, 11, 14, 16, 22] {
            let decision = decide_recovery(today(), None, hour * 3600, false);
            assert_eq!(decision, RecoveryDecision::SkipPhase2OutsideMarketHours);
        }
    }

    #[test]
    fn test_decide_pre_open_non_trading_day_skips() {
        // Saturday morning crash-restart before "market open" — no snapshot.
        let decision = decide_recovery(today(), None, 7 * 3600, false);
        assert_eq!(decision, RecoveryDecision::SkipPhase2OutsideMarketHours);
    }

    #[test]
    fn test_decide_post_close_stale_snapshot_skips() {
        // 16:00 IST, prior-day snapshot → skip. Subscribe-later is safer
        // than resurrecting yesterday's ATM.
        let snap = sample_snapshot(yesterday());
        let decision = decide_recovery(today(), Some(&snap), 16 * 3600, true);
        assert_eq!(decision, RecoveryDecision::SkipPhase2OutsideMarketHours);
    }

    #[test]
    fn test_read_snapshot_returns_none_when_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-phase2-missing-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(read_snapshot(&tmp).unwrap().is_none());
    }

    #[test]
    fn test_write_then_read_snapshot_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-phase2-roundtrip-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let snap = sample_snapshot(today());
        write_snapshot(&tmp, &snap).unwrap();
        let read = read_snapshot(&tmp).unwrap().unwrap();
        assert_eq!(read, snap);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_write_snapshot_creates_parent_directory() {
        let base = std::env::temp_dir().join(format!(
            "tv-phase2-parent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let tmp = base.join("nested/dir/phase2.json");
        write_snapshot(&tmp, &sample_snapshot(today())).unwrap();
        assert!(tmp.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_read_snapshot_returns_parse_error_on_corrupt_file() {
        let tmp = std::env::temp_dir().join(format!(
            "tv-phase2-corrupt-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, b"not json at all").unwrap();
        let err = read_snapshot(&tmp).unwrap_err();
        assert!(matches!(err, SubscriptionMarkerError::Parse { .. }));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_constants_match_spec() {
        assert_eq!(PHASE2_TRIGGER_SECS_IST, 9 * 3600 + 12 * 60);
        assert_eq!(MARKET_CLOSE_SECS_IST, 15 * 3600 + 30 * 60);
        // 09:12 < 15:30 — obvious invariant.
        assert!(PHASE2_TRIGGER_SECS_IST < MARKET_CLOSE_SECS_IST);
    }

    #[test]
    fn test_recovery_decision_variants_are_distinct() {
        use RecoveryDecision::*;
        let a = RunFreshPhase2;
        let b = WaitForPhase2Scheduler;
        let c = SkipPhase2OutsideMarketHours;
        let d = UseSnapshot {
            snapshot: sample_snapshot(today()),
        };
        // Cross-variant inequality.
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_ne!(a, d);
        assert_ne!(b, d);
        assert_ne!(c, d);
    }

    #[test]
    fn test_persisted_subscription_serde_roundtrip() {
        let p = PersistedSubscription {
            exchange_segment: "NSE_FNO".to_string(),
            security_id: "49123".to_string(),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PersistedSubscription = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }
}
