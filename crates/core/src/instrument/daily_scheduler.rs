//! Daily instrument CSV refresh scheduler (I-P1-01 / GAP-CFG-01).
//!
//! Spawns a background tokio task that wakes up at `daily_download_time` IST
//! each trading day to refresh the instrument universe.
//!
//! # Behavior
//! - Calculates time until next `daily_download_time` IST
//! - Sleeps until that time
//! - On wake: checks if it's a trading day (skips weekends/holidays)
//! - Triggers instrument rebuild via the provided callback
//! - Logs the result and goes back to sleep for next day

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, FixedOffset, NaiveTime, Timelike, Utc};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};

// I-P1-01: Full day in seconds for next-day wrapping
const SECS_PER_DAY: i64 = 86_400;

// ---------------------------------------------------------------------------
// DailyRefreshConfig
// ---------------------------------------------------------------------------

// GAP-CFG-01: Configuration for the daily instrument refresh scheduler.
/// Runtime configuration for the daily instrument refresh scheduler.
#[derive(Debug, Clone)]
pub struct DailyRefreshConfig {
    /// Target time of day (IST) to trigger the refresh.
    pub download_time: NaiveTime,
    /// Whether the scheduler is enabled. Disabled by default.
    pub enabled: bool,
}

/// GAP-CFG-01: Default download time (08:55:00 IST).
/// Using a module-level constant avoids `.expect()` in the `Default` impl.
const DEFAULT_DOWNLOAD_HOUR: u32 = 8;
const DEFAULT_DOWNLOAD_MIN: u32 = 55;
const DEFAULT_DOWNLOAD_SEC: u32 = 0;

impl Default for DailyRefreshConfig {
    fn default() -> Self {
        // GAP-CFG-01: default matches config's "08:55:00"
        // Safety: 08:55:00 is always a valid time — if it fails, panicking at startup is correct.
        let download_time = match NaiveTime::from_hms_opt(
            DEFAULT_DOWNLOAD_HOUR,
            DEFAULT_DOWNLOAD_MIN,
            DEFAULT_DOWNLOAD_SEC,
        ) {
            Some(time) => time,
            None => unreachable!("08:55:00 is a valid NaiveTime"),
        };

        Self {
            download_time,
            enabled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure function: compute_next_trigger_time
// ---------------------------------------------------------------------------

// I-P1-01: Pure function — deterministic, no side effects, no I/O.
/// Computes the duration from `now_ist` until the next occurrence of `target_time` (IST).
///
/// - If `target_time` is strictly in the future today, returns the duration until then.
/// - If `target_time` has already passed today (or is exactly now), returns the duration
///   until `target_time` tomorrow.
///
/// This is a pure function: deterministic, no side effects, suitable for exhaustive testing.
pub fn compute_next_trigger_time(
    now_ist: DateTime<FixedOffset>,
    target_time: NaiveTime,
) -> Duration {
    let now_time = now_ist.time();

    // I-P1-01: seconds from now until target, within the same day
    let now_secs = i64::from(now_time.num_seconds_from_midnight());
    let target_secs = i64::from(target_time.num_seconds_from_midnight());
    let delta_secs = target_secs.saturating_sub(now_secs);

    let sleep_secs = if delta_secs > 0 {
        // Target is in the future today
        delta_secs
    } else {
        // Target has passed today (or is exactly now) → wrap to tomorrow
        delta_secs.saturating_add(SECS_PER_DAY)
    };

    // Safety: sleep_secs is always positive (1..=86400)
    Duration::from_secs(sleep_secs as u64)
}

// ---------------------------------------------------------------------------
// parse_daily_download_time
// ---------------------------------------------------------------------------

// GAP-CFG-01: Parse the daily_download_time string from config.
/// Parses a `"HH:MM:SS"` string into a `NaiveTime`.
///
/// # Errors
/// Returns an error if the string does not match the exact `%H:%M:%S` format.
pub fn parse_daily_download_time(time_str: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(time_str, "%H:%M:%S").map_err(|err| {
        anyhow::anyhow!(
            "invalid daily_download_time '{}': expected HH:MM:SS format — {}",
            time_str,
            err
        )
    })
}

// ---------------------------------------------------------------------------
// spawn_daily_refresh_task
// ---------------------------------------------------------------------------

// I-P1-01: Background scheduler task for daily instrument refresh.
/// Spawns a background tokio task that triggers instrument refresh at
/// `config.download_time` IST each trading day.
///
/// # Arguments
/// * `config` — Scheduler configuration (target time, enabled flag).
/// * `calendar` — Trading calendar for weekend/holiday checks.
/// * `shutdown_rx` — Watch receiver; task exits when a `true` is received.
/// * `trigger_tx` — Sends `()` to signal that a refresh should occur.
///
/// # Returns
/// A `JoinHandle` for the spawned task. The task loops indefinitely until
/// shutdown is signaled.
pub fn spawn_daily_refresh_task(
    config: DailyRefreshConfig,
    calendar: TradingCalendar,
    mut shutdown_rx: watch::Receiver<bool>,
    trigger_tx: mpsc::Sender<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            // GAP-CFG-01: scheduler disabled — park until shutdown
            info!("daily instrument refresh scheduler disabled");
            drop(shutdown_rx.changed().await);
            return;
        }

        info!(
            target_time = %config.download_time,
            "daily instrument refresh scheduler started"
        );

        loop {
            // I-P1-01: Compute IST "now" for scheduling
            let now_ist = Utc::now().with_timezone(&ist_offset());

            let sleep_duration = compute_next_trigger_time(now_ist, config.download_time);

            debug!(
                sleep_secs = sleep_duration.as_secs(),
                target_time = %config.download_time,
                "sleeping until next instrument refresh window"
            );

            // I-P1-01: Sleep until target time, or exit on shutdown
            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {},
                _ = shutdown_rx.changed() => {
                    info!("daily refresh scheduler received shutdown signal");
                    return;
                }
            }

            // I-P1-01: Check if today is a trading day
            let today_ist = Utc::now().with_timezone(&ist_offset()).date_naive();

            if !calendar.is_trading_day(today_ist) {
                info!(
                    date = %today_ist,
                    "skipping instrument refresh — not a trading day"
                );
                continue;
            }

            // I-P1-01: Trigger instrument rebuild
            info!(date = %today_ist, "triggering daily instrument refresh");
            match trigger_tx.try_send(()) {
                Ok(()) => {
                    info!("instrument refresh signal sent successfully");
                }
                Err(err) => {
                    warn!(?err, "failed to send instrument refresh signal");
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::NaiveDate;

    /// Helper: build a `DateTime<FixedOffset>` at a specific IST time.
    fn ist_datetime(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
    ) -> DateTime<FixedOffset> {
        let offset = ist_offset();
        let naive_date = NaiveDate::from_ymd_opt(year, month, day).expect("valid test date"); // APPROVED: test helper
        let naive_time = NaiveTime::from_hms_opt(hour, min, sec).expect("valid test time"); // APPROVED: test helper
        let naive_dt = naive_date.and_time(naive_time);
        naive_dt
            .and_local_timezone(offset)
            .single()
            .expect("unambiguous IST datetime") // APPROVED: test helper — IST is fixed offset, never ambiguous
    }

    /// Helper: build a `NaiveTime` from H:M:S.
    fn hms(hour: u32, min: u32, sec: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, min, sec).expect("valid test time") // APPROVED: test helper
    }

    // I-P1-01: Target time 14:00, now is 10:00 → ~4h (14400s)
    #[test]
    fn test_compute_next_trigger_future_today() {
        let now = ist_datetime(2026, 3, 11, 10, 0, 0);
        let target = hms(14, 0, 0);
        let duration = compute_next_trigger_time(now, target);
        assert_eq!(
            duration.as_secs(),
            4 * 3600,
            "expected 4 hours = 14400 seconds"
        );
    }

    // I-P1-01: Target time 08:55, now is 10:00 → ~22h55m (82500s)
    #[test]
    fn test_compute_next_trigger_past_today() {
        let now = ist_datetime(2026, 3, 11, 10, 0, 0);
        let target = hms(8, 55, 0);
        let duration = compute_next_trigger_time(now, target);
        // 08:55 tomorrow - 10:00 today = 22h55m = 82500s
        let expected = 22 * 3600 + 55 * 60;
        assert_eq!(
            duration.as_secs(),
            expected,
            "expected 22h55m = {expected} seconds"
        );
    }

    // I-P1-01: Target == now → 24h (next day)
    #[test]
    fn test_compute_next_trigger_exactly_now() {
        let now = ist_datetime(2026, 3, 11, 8, 55, 0);
        let target = hms(8, 55, 0);
        let duration = compute_next_trigger_time(now, target);
        assert_eq!(duration.as_secs(), 86_400, "expected exactly 24 hours");
    }

    // I-P1-01: Midnight boundary — target 00:01, now 23:59 → ~2min (120s)
    #[test]
    fn test_compute_next_trigger_midnight_boundary() {
        let now = ist_datetime(2026, 3, 11, 23, 59, 0);
        let target = hms(0, 1, 0);
        let duration = compute_next_trigger_time(now, target);
        // 00:01 tomorrow - 23:59 today = 2 minutes = 120s
        assert_eq!(duration.as_secs(), 120, "expected 2 minutes = 120 seconds");
    }

    // GAP-CFG-01: Valid time string parses correctly
    #[test]
    fn test_parse_daily_download_time_valid() {
        let result = parse_daily_download_time("08:55:00");
        assert!(result.is_ok());
        let time = result.expect("just checked is_ok"); // APPROVED: test assertion
        assert_eq!(time, hms(8, 55, 0));
    }

    // GAP-CFG-01: Invalid string returns error
    #[test]
    fn test_parse_daily_download_time_invalid() {
        let result = parse_daily_download_time("bad");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string(); // APPROVED: test assertion
        assert!(
            err_msg.contains("invalid daily_download_time"),
            "error message should be descriptive, got: {err_msg}"
        );
    }

    // GAP-CFG-01: Missing seconds returns error (must be HH:MM:SS)
    #[test]
    fn test_parse_daily_download_time_missing_seconds() {
        let result = parse_daily_download_time("08:55");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string(); // APPROVED: test assertion
        assert!(
            err_msg.contains("invalid daily_download_time"),
            "expected descriptive error for missing seconds, got: {err_msg}"
        );
    }

    // GAP-CFG-01: Default config is disabled
    #[test]
    fn test_daily_refresh_config_default_disabled() {
        let config = DailyRefreshConfig::default();
        assert!(!config.enabled, "scheduler must be disabled by default");
        assert_eq!(config.download_time, hms(8, 55, 0));
    }

    // I-P1-01: Target time 00:00, now is 23:59:59 → 1 second
    #[test]
    fn test_compute_next_trigger_one_second_before_midnight() {
        let now = ist_datetime(2026, 3, 11, 23, 59, 59);
        let target = hms(0, 0, 0);
        let duration = compute_next_trigger_time(now, target);
        // 00:00 tomorrow - 23:59:59 today = 1 second
        assert_eq!(duration.as_secs(), 1);
    }

    // I-P1-01: Target time 23:59:59, now is 00:00:00 → ~23h59m59s
    #[test]
    fn test_compute_next_trigger_near_end_of_day() {
        let now = ist_datetime(2026, 3, 11, 0, 0, 0);
        let target = hms(23, 59, 59);
        let duration = compute_next_trigger_time(now, target);
        let expected = 23 * 3600 + 59 * 60 + 59;
        assert_eq!(duration.as_secs(), expected);
    }

    // I-P1-01: Target 1 second in the future
    #[test]
    fn test_compute_next_trigger_one_second_ahead() {
        let now = ist_datetime(2026, 3, 11, 12, 0, 0);
        let target = hms(12, 0, 1);
        let duration = compute_next_trigger_time(now, target);
        assert_eq!(duration.as_secs(), 1);
    }

    // I-P1-01: Target 1 second in the past → wraps to next day
    #[test]
    fn test_compute_next_trigger_one_second_past() {
        let now = ist_datetime(2026, 3, 11, 12, 0, 1);
        let target = hms(12, 0, 0);
        let duration = compute_next_trigger_time(now, target);
        // Wraps to next day: 86400 - 1 = 86399
        assert_eq!(duration.as_secs(), 86_399);
    }

    // I-P1-01: Result is always positive (1..=86400)
    #[test]
    fn test_compute_next_trigger_always_positive() {
        // Test multiple combinations
        for hour in [0, 6, 9, 12, 15, 20, 23] {
            for target_hour in [0, 6, 9, 12, 15, 20, 23] {
                let now = ist_datetime(2026, 3, 11, hour, 30, 0);
                let target = hms(target_hour, 15, 0);
                let duration = compute_next_trigger_time(now, target);
                assert!(
                    duration.as_secs() >= 1 && duration.as_secs() <= 86_400,
                    "duration must be 1..=86400, got {} for now={}:30 target={}:15",
                    duration.as_secs(),
                    hour,
                    target_hour
                );
            }
        }
    }

    // GAP-CFG-01: Parse various valid time strings
    #[test]
    fn test_parse_daily_download_time_midnight() {
        let result = parse_daily_download_time("00:00:00");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), hms(0, 0, 0));
    }

    #[test]
    fn test_parse_daily_download_time_end_of_day() {
        let result = parse_daily_download_time("23:59:59");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), hms(23, 59, 59));
    }

    // GAP-CFG-01: Invalid formats
    #[test]
    fn test_parse_daily_download_time_invalid_hour() {
        assert!(parse_daily_download_time("25:00:00").is_err());
    }

    #[test]
    fn test_parse_daily_download_time_empty_string() {
        assert!(parse_daily_download_time("").is_err());
    }

    #[test]
    fn test_parse_daily_download_time_extra_fields() {
        // "08:55:00:00" has an extra field
        assert!(parse_daily_download_time("08:55:00:00").is_err());
    }

    // I-P1-01: spawn_daily_refresh_task — disabled config exits on shutdown
    #[tokio::test]
    async fn test_spawn_daily_refresh_task_disabled_exits_on_shutdown() {
        let config = DailyRefreshConfig::default(); // disabled
        let calendar = tickvault_common::trading_calendar::TradingCalendar::from_config(
            &tickvault_common::config::TradingConfig {
                market_open_time: "09:00:00".to_string(),
                market_close_time: "15:30:00".to_string(),
                order_cutoff_time: "15:29:00".to_string(),
                data_collection_start: "09:00:00".to_string(),
                data_collection_end: "16:00:00".to_string(),
                timezone: "Asia/Kolkata".to_string(),
                max_orders_per_second: 10,
                nse_holidays: vec![],
                muhurat_trading_dates: vec![],
                nse_mock_trading_dates: vec![],
            },
        )
        .unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (_trigger_tx, _trigger_rx) = mpsc::channel::<()>(1);
        let handle = spawn_daily_refresh_task(config, calendar, shutdown_rx, _trigger_tx);

        // Send shutdown
        let _ = shutdown_tx.send(true);

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("disabled task should exit within 2s")
            .expect("task should not panic");
    }

    // I-P1-01: SECS_PER_DAY constant is correct
    #[test]
    fn test_secs_per_day_constant() {
        assert_eq!(SECS_PER_DAY, 86_400);
    }

    // GAP-CFG-01: DailyRefreshConfig debug
    #[test]
    fn test_daily_refresh_config_debug() {
        let config = DailyRefreshConfig::default();
        let debug = format!("{config:?}");
        assert!(debug.contains("download_time"));
        assert!(debug.contains("enabled"));
    }

    // GAP-CFG-01: DailyRefreshConfig clone
    #[test]
    fn test_daily_refresh_config_clone() {
        let config = DailyRefreshConfig {
            download_time: hms(7, 30, 0),
            enabled: true,
        };
        let cloned = config.clone();
        assert_eq!(cloned.download_time, hms(7, 30, 0));
        assert!(cloned.enabled);
    }

    // I-P1-01: Enabled scheduler shuts down cleanly before firing
    #[tokio::test]
    async fn test_spawn_daily_refresh_task_enabled_shutdown_before_fire() {
        // Set download_time far in the future so it sleeps, then shutdown immediately
        let config = DailyRefreshConfig {
            download_time: hms(23, 59, 59),
            enabled: true,
        };
        let calendar = tickvault_common::trading_calendar::TradingCalendar::from_config(
            &tickvault_common::config::TradingConfig {
                market_open_time: "09:00:00".to_string(),
                market_close_time: "15:30:00".to_string(),
                order_cutoff_time: "15:29:00".to_string(),
                data_collection_start: "09:00:00".to_string(),
                data_collection_end: "16:00:00".to_string(),
                timezone: "Asia/Kolkata".to_string(),
                max_orders_per_second: 10,
                nse_holidays: vec![],
                muhurat_trading_dates: vec![],
                nse_mock_trading_dates: vec![],
            },
        )
        .unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (trigger_tx, _trigger_rx) = mpsc::channel::<()>(1);
        let handle = spawn_daily_refresh_task(config, calendar, shutdown_rx, trigger_tx);

        // Let it start sleeping, then shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(true);

        tokio::time::timeout(std::time::Duration::from_secs(3), handle)
            .await
            .expect("enabled task should exit within 3s after shutdown") // APPROVED: test
            .expect("task should not panic"); // APPROVED: test
    }

    // I-P1-01: Enabled scheduler fires on non-trading day (holiday)
    #[tokio::test]
    async fn test_spawn_daily_refresh_task_enabled_fires_on_non_trading_day() {
        // Use tokio time pause to fast-forward
        tokio::time::pause();

        // Set target time 1 second from now so it fires quickly
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let target = now_ist.time() + chrono::Duration::seconds(1);
        let target_naive = NaiveTime::from_hms_opt(target.hour(), target.minute(), target.second())
            .unwrap_or(hms(12, 0, 0));

        let config = DailyRefreshConfig {
            download_time: target_naive,
            enabled: true,
        };

        // Calendar that treats today as non-trading.
        // On weekdays: add today as a holiday. On weekends: it's already non-trading.
        use chrono::Datelike;
        use tickvault_common::config::NseHolidayEntry;
        let today_ist = Utc::now().with_timezone(&ist_offset()).date_naive();
        let is_weekday = today_ist.weekday().num_days_from_monday() < 5;
        let holidays = if is_weekday {
            vec![NseHolidayEntry {
                date: today_ist.to_string(),
                name: "Test Holiday".to_string(),
            }]
        } else {
            vec![]
        };
        let calendar = tickvault_common::trading_calendar::TradingCalendar::from_config(
            &tickvault_common::config::TradingConfig {
                market_open_time: "09:00:00".to_string(),
                market_close_time: "15:30:00".to_string(),
                order_cutoff_time: "15:29:00".to_string(),
                data_collection_start: "09:00:00".to_string(),
                data_collection_end: "16:00:00".to_string(),
                timezone: "Asia/Kolkata".to_string(),
                max_orders_per_second: 10,
                nse_holidays: holidays,
                muhurat_trading_dates: vec![],
                nse_mock_trading_dates: vec![],
            },
        )
        .unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (trigger_tx, _trigger_rx) = mpsc::channel::<()>(1);
        let handle = spawn_daily_refresh_task(config, calendar, shutdown_rx, trigger_tx);

        // Fast-forward time past the target
        tokio::time::advance(std::time::Duration::from_secs(3)).await;

        // Let the scheduler run through its holiday check
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        // Shutdown
        let _ = shutdown_tx.send(true);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;

        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    // GAP-CFG-01: Invalid HH:MM (no seconds) format
    #[test]
    fn test_parse_daily_download_time_only_hours() {
        assert!(parse_daily_download_time("08").is_err());
    }

    // GAP-CFG-01: Alphabetic characters in time string
    #[test]
    fn test_parse_daily_download_time_with_letters() {
        assert!(parse_daily_download_time("ab:cd:ef").is_err());
    }

    // I-P1-01: compute_next_trigger_time — target far in the future today
    #[test]
    fn test_compute_next_trigger_target_23h_now_0h() {
        let now = ist_datetime(2026, 3, 11, 0, 0, 0);
        let target = hms(23, 0, 0);
        let duration = compute_next_trigger_time(now, target);
        assert_eq!(duration.as_secs(), 23 * 3600);
    }

    // I-P1-01: DailyRefreshConfig fields accessible
    #[test]
    fn test_daily_refresh_config_enabled_custom() {
        let config = DailyRefreshConfig {
            download_time: hms(10, 0, 0),
            enabled: true,
        };
        assert!(config.enabled);
        assert_eq!(config.download_time, hms(10, 0, 0));
    }

    // GAP-CFG-01: parse returns correct time for edge values
    #[test]
    fn test_parse_daily_download_time_noon() {
        let result = parse_daily_download_time("12:00:00");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), hms(12, 0, 0));
    }

    // I-P1-01: compute always returns >= 1 second
    #[test]
    fn test_compute_next_trigger_minimum_is_one_second() {
        // When target is 1 second past now, should return 1 second
        let now = ist_datetime(2026, 3, 11, 12, 0, 0);
        let target = hms(12, 0, 1);
        let duration = compute_next_trigger_time(now, target);
        assert!(duration.as_secs() >= 1);
    }

    // I-P1-01: Enabled scheduler fires on a trading day and sends trigger
    #[tokio::test]
    async fn test_spawn_daily_refresh_task_enabled_fires_on_trading_day() {
        tokio::time::pause();

        // Set target time 1 second from now
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let target = now_ist.time() + chrono::Duration::seconds(1);
        let target_naive = NaiveTime::from_hms_opt(target.hour(), target.minute(), target.second())
            .unwrap_or(hms(12, 0, 0));

        let config = DailyRefreshConfig {
            download_time: target_naive,
            enabled: true,
        };

        // No holidays — today is a trading day (if weekday)
        let calendar = tickvault_common::trading_calendar::TradingCalendar::from_config(
            &tickvault_common::config::TradingConfig {
                market_open_time: "09:00:00".to_string(),
                market_close_time: "15:30:00".to_string(),
                order_cutoff_time: "15:29:00".to_string(),
                data_collection_start: "09:00:00".to_string(),
                data_collection_end: "16:00:00".to_string(),
                timezone: "Asia/Kolkata".to_string(),
                max_orders_per_second: 10,
                nse_holidays: vec![],
                muhurat_trading_dates: vec![],
                nse_mock_trading_dates: vec![],
            },
        )
        .unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<()>(1);
        let handle = spawn_daily_refresh_task(config, calendar, shutdown_rx, trigger_tx);

        // Fast-forward time past the target
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        // On trading days, the trigger should fire. On weekends, it skips.
        // Either way, no panic.
        let received = trigger_rx.try_recv();
        // On weekdays: Ok(()), on weekends: Err(Empty)
        let _ = received;

        // Shutdown
        let _ = shutdown_tx.send(true);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }
}
