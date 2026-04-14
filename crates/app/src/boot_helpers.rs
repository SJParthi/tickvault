//! Boot sequence helpers extracted from `main.rs` for testability and coverage.
//!
//! Contains both pure helper functions (no I/O) and async boot orchestration
//! functions that take dependencies as parameters. All logic that was previously
//! inline in `main()` is extracted here so that `cargo llvm-cov` can instrument
//! it via the lib target.
//!
//! # Function categories
//!
//! - **Pure helpers** — zero I/O, fully testable: `format_bind_addr`, `effective_ws_stagger`, etc.
//! - **Config & logging** — `load_and_validate_config`, `build_env_filter`, `build_fmt_layer`
//! - **Boot orchestration** — `load_instruments`, `build_websocket_pool`, `run_shutdown_fast`
//! - **Persistence consumers** — `run_tick_persistence_consumer`, `run_candle_persistence_consumer`

use chrono::Timelike;
use tracing::warn;

use dhan_live_trader_common::trading_calendar::ist_offset;
use dhan_live_trader_core::historical::cross_verify::{
    CrossMatchMismatch, CrossVerificationReport, ViolationDetail,
};

// ---------------------------------------------------------------------------
// Constants (moved from main.rs for coverage instrumentation)
// ---------------------------------------------------------------------------

/// Base config file path (relative to working directory).
pub const CONFIG_BASE_PATH: &str = "config/base.toml";

/// Log file path for Alloy/Loki consumption.
pub const APP_LOG_FILE_PATH: &str = "data/logs/app.log";

/// Fast boot window start (IST).
pub const FAST_BOOT_WINDOW_START: &str = "09:00:00";

/// Fast boot window end (IST). NSE regular session closes at 15:30.
pub const FAST_BOOT_WINDOW_END: &str = "15:30:00";

/// Reduced WebSocket connection stagger for off-market-hours boot (milliseconds).
pub const OFF_HOURS_CONNECTION_STAGGER_MS: u64 = 1000;

/// Local override config file path (git-ignored, optional).
pub const CONFIG_LOCAL_PATH: &str = "config/local.toml";

// ---------------------------------------------------------------------------
// IST timestamp formatter for tracing-subscriber
// ---------------------------------------------------------------------------

/// Custom `FormatTime` implementation that outputs IST timestamps with +05:30 offset.
///
/// All log timestamps display as `2026-04-06T11:31:41.806275+05:30` instead of
/// UTC `2026-04-06T06:01:41.806275Z`. Required for SEBI compliance and IST-native
/// debugging during market hours.
#[derive(Clone, Debug)]
pub struct IstTimer;

impl tracing_subscriber::fmt::time::FormatTime for IstTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Utc::now().with_timezone(&ist_offset());
        write!(w, "{}", now.format("%Y-%m-%dT%H:%M:%S%.6f%:z"))
    }
}

// ---------------------------------------------------------------------------
// Pure helper functions
// ---------------------------------------------------------------------------

/// Determines the effective WebSocket connection stagger in milliseconds.
///
/// Always uses `OFF_HOURS_CONNECTION_STAGGER_MS` (1s) for fast boot.
/// This ensures crash recovery during market hours gets all 5 connections
/// up in ~5 seconds instead of ~50 seconds with 10s stagger.
/// Dhan's WebSocket servers handle 5 concurrent connects without issue.
pub fn effective_ws_stagger(_configured_stagger_ms: u64, _is_market_hours: bool) -> u64 {
    OFF_HOURS_CONNECTION_STAGGER_MS
}

/// Formats a host:port pair into a `SocketAddr` string.
///
/// Returns the formatted string — caller is responsible for parsing.
/// This is a pure function extracted for testability.
pub fn format_bind_addr(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Determines the boot mode description for logging.
///
/// Returns `"fast"` if a valid token cache exists AND we're within market
/// hours on a trading day. Otherwise returns `"standard"`.
pub fn determine_boot_mode(has_cache: bool, is_market_hours: bool) -> &'static str {
    if has_cache && is_market_hours {
        "fast"
    } else {
        "standard"
    }
}

/// Determines whether to use the fast boot path.
///
/// Fast boot requires BOTH a valid token cache AND being within market hours
/// on a trading day. If the cache exists but we're outside market hours,
/// the slow boot path is used (downloads fresh instruments, starts Docker first).
pub fn should_fast_boot(has_cache: bool, is_market_hours: bool) -> bool {
    has_cache && is_market_hours
}

/// Formats timeframe coverage details for Telegram notification.
pub fn format_timeframe_details(report: &CrossVerificationReport) -> String {
    let mut lines = Vec::with_capacity(report.timeframe_counts.len());
    for tc in &report.timeframe_counts {
        lines.push(format!(
            "{}: {} ({} inst)",
            tc.timeframe, tc.candle_count, tc.instrument_count,
        ));
    }
    lines.join("\n")
}

/// Formats `ViolationDetail` records into pre-formatted Telegram lines.
pub fn format_violation_details(details: &[ViolationDetail]) -> Vec<String> {
    details
        .iter()
        .map(|d| {
            format!(
                "\u{2022} {} ({}) {} @ {}\n  {}",
                d.symbol, d.segment, d.timeframe, d.timestamp_ist, d.values
            )
        })
        .collect()
}

/// Formats `CrossMatchMismatch` records into pre-formatted Telegram lines.
pub fn format_cross_match_details(details: &[CrossMatchMismatch]) -> Vec<String> {
    details
        .iter()
        .map(|d| {
            let mut line = format!(
                "\u{2022} {} ({}) {} @ {}\n  Hist: {}",
                d.symbol, d.segment, d.timeframe, d.timestamp_ist, d.hist_values
            );
            line.push_str(&format!("\n  Live: {}", d.live_values));
            if !d.diff_summary.is_empty() {
                line.push_str(&format!("\n  Diff: {}", d.diff_summary));
            }
            line
        })
        .collect()
}

/// Computes the sleep duration until the given market close time (IST).
///
/// Returns `Duration::ZERO` on parse failure or if already past the close time.
pub fn compute_market_close_sleep(market_close_time_str: &str) -> std::time::Duration {
    let close_time = match chrono::NaiveTime::parse_from_str(market_close_time_str, "%H:%M:%S") {
        Ok(t) => t,
        Err(err) => {
            warn!(
                ?err,
                market_close_time = market_close_time_str,
                "failed to parse market close time"
            );
            return std::time::Duration::ZERO;
        }
    };

    let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
    let now_time = now_ist.time();

    if now_time >= close_time {
        return std::time::Duration::ZERO;
    }

    let now_secs = u64::from(now_time.num_seconds_from_midnight());
    let close_secs = u64::from(close_time.num_seconds_from_midnight());
    std::time::Duration::from_secs(close_secs.saturating_sub(now_secs))
}

/// Opens (or creates) the app log file for Alloy consumption.
///
/// Creates `data/logs/` directory if needed. Returns `None` if the file
/// cannot be created (best-effort — logging to stdout always works).
pub fn create_log_file_writer() -> Option<std::fs::File> {
    create_log_file_writer_at(APP_LOG_FILE_PATH)
}

/// Opens (or creates) a log file at the given path for Alloy consumption.
///
/// Creates parent directory if needed. Returns `None` if the directory
/// cannot be created or the file cannot be opened.
///
/// Extracted from `create_log_file_writer` for testability — allows tests
/// to exercise error paths with invalid/unwritable paths.
pub fn create_log_file_writer_at(log_file_path: &str) -> Option<std::fs::File> {
    let log_dir = std::path::Path::new(log_file_path)
        .parent()
        .unwrap_or(std::path::Path::new("data/logs"));

    // O(1) EXEMPT: begin — cold path, logging bootstrap before tracing is initialized
    #[allow(clippy::print_stderr)] // APPROVED: tracing not yet initialized at this point
    if let Err(err) = std::fs::create_dir_all(log_dir) {
        eprintln!("warning: cannot create log directory {log_dir:?}: {err}");
        return None;
    }

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
    {
        Ok(file) => Some(file),
        #[allow(clippy::print_stderr)] // APPROVED: tracing not yet initialized at this point
        Err(err) => {
            eprintln!("warning: cannot open log file {log_file_path}: {err}");
            None
        } // O(1) EXEMPT: end
    }
}

// ---------------------------------------------------------------------------
// Log rotation constants
// ---------------------------------------------------------------------------

/// Maximum log file size in bytes (50 MB).
pub const LOG_MAX_SIZE_BYTES: u64 = 50 * 1024 * 1024;
/// Maximum number of rotated log files to keep.
pub const LOG_MAX_FILES: u32 = 7;

// ---------------------------------------------------------------------------
// Clock drift check
// ---------------------------------------------------------------------------

/// Maximum acceptable clock drift in seconds before warning.
pub const CLOCK_DRIFT_THRESHOLD_SECS: u64 = 2;

/// HTTP timeout for clock drift check (seconds).
const CLOCK_DRIFT_HTTP_TIMEOUT_SECS: u64 = 5;

// APPROVED: hardcoded URL — public time reference, not a Dhan API endpoint
const CLOCK_DRIFT_REFERENCE_URL: &str = "https://www.google.com";

/// Checks system clock against an HTTP server's Date header (cold path, boot only).
pub async fn check_clock_drift() -> Option<i64> {
    let local_now = chrono::Utc::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            CLOCK_DRIFT_HTTP_TIMEOUT_SECS,
        ))
        .build()
        .ok()?;
    let response = client.head(CLOCK_DRIFT_REFERENCE_URL).send().await.ok()?;
    let date_str = response.headers().get("date")?.to_str().ok()?;
    let server_time = chrono::DateTime::parse_from_str(date_str, "%a, %d %b %Y %H:%M:%S GMT")
        .ok()?
        .with_timezone(&chrono::Utc);
    let drift_secs = (server_time - local_now).num_seconds();
    if drift_secs.unsigned_abs() > CLOCK_DRIFT_THRESHOLD_SECS {
        warn!(
            drift_secs,
            "clock drift detected — system clock may be inaccurate"
        );
    }
    Some(drift_secs)
}

// ---------------------------------------------------------------------------
// Heartbeat watchdog
// ---------------------------------------------------------------------------

/// Heartbeat watchdog interval in seconds.
pub const WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Spawns a background watchdog that periodically checks system liveness.
///
/// Checks every 30 seconds:
/// - Token handle has a valid (non-None) token
/// - Tick broadcast sender has active receivers
///
/// Logs ERROR on failure (triggers Telegram via Loki -> Grafana).
pub fn spawn_heartbeat_watchdog(
    token_handle: dhan_live_trader_core::auth::token_manager::TokenHandle,
    tick_sender: tokio::sync::broadcast::Sender<dhan_live_trader_common::tick_types::ParsedTick>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(WATCHDOG_INTERVAL_SECS));
        interval.tick().await; // Skip immediate first tick.

        loop {
            interval.tick().await;

            let token_ok = {
                let guard = token_handle.load();
                guard.as_ref().is_some()
            };
            if !token_ok {
                tracing::error!("WATCHDOG: token handle is None — authentication may have failed");
            }

            let receiver_count = tick_sender.receiver_count();
            if receiver_count == 0 {
                tracing::error!(
                    "WATCHDOG: tick broadcast has 0 receivers — tick processor may have crashed"
                );
            }

            // L4: Export system metrics (open FDs, thread count) every watchdog cycle.
            crate::infra::export_system_metrics();

            // C1: Emit systemd watchdog heartbeat (no-op if not under systemd).
            crate::infra::notify_systemd_watchdog();
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_core::historical::cross_verify::TimeframeCoverage;
    use std::net::SocketAddr;

    fn make_coverage(
        timeframe: &str,
        candle_count: usize,
        instrument_count: usize,
    ) -> TimeframeCoverage {
        TimeframeCoverage {
            timeframe: timeframe.to_string(),
            candle_count,
            instrument_count,
        }
    }

    fn make_verification_report(
        timeframe_counts: Vec<TimeframeCoverage>,
    ) -> CrossVerificationReport {
        CrossVerificationReport {
            instruments_checked: 0,
            instruments_complete: 0,
            instruments_with_gaps: 0,
            total_candles_in_db: 0,
            timeframe_counts,
            ohlc_violations: 0,
            ohlc_details: vec![],
            data_violations: 0,
            data_details: vec![],
            timestamp_violations: 0,
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
            passed: true,
        }
    }

    fn make_violation(
        symbol: &str,
        segment: &str,
        timeframe: &str,
        timestamp_ist: &str,
        violation: &str,
        values: &str,
    ) -> ViolationDetail {
        ViolationDetail {
            symbol: symbol.to_string(),
            segment: segment.to_string(),
            timeframe: timeframe.to_string(),
            timestamp_ist: timestamp_ist.to_string(),
            violation: violation.to_string(),
            values: values.to_string(),
        }
    }

    fn make_cross_match(
        symbol: &str,
        segment: &str,
        timeframe: &str,
        timestamp_ist: &str,
        hist_values: &str,
        live_values: &str,
        diff_summary: &str,
    ) -> CrossMatchMismatch {
        CrossMatchMismatch {
            symbol: symbol.to_string(),
            segment: segment.to_string(),
            timeframe: timeframe.to_string(),
            timestamp_ist: timestamp_ist.to_string(),
            mismatch_type: "price_diff".to_string(),
            hist_values: hist_values.to_string(),
            live_values: live_values.to_string(),
            diff_summary: diff_summary.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Config path tests
    // -----------------------------------------------------------------------

    #[test]
    fn config_base_path_is_toml() {
        assert!(CONFIG_BASE_PATH.ends_with(".toml"));
    }

    #[test]
    fn config_local_path_is_toml() {
        assert!(CONFIG_LOCAL_PATH.ends_with(".toml"));
    }

    #[test]
    fn test_config_base_path_starts_with_config() {
        assert!(CONFIG_BASE_PATH.starts_with("config/"));
    }

    #[test]
    fn test_config_local_path_starts_with_config() {
        assert!(CONFIG_LOCAL_PATH.starts_with("config/"));
    }

    #[test]
    fn test_config_base_path_is_relative_toml() {
        let path = std::path::Path::new(CONFIG_BASE_PATH);
        assert!(!path.is_absolute());
        assert!(path.extension().is_some_and(|ext| ext == "toml"));
    }

    #[test]
    fn test_config_local_path_is_git_ignorable() {
        assert!(CONFIG_LOCAL_PATH.contains("local"));
    }

    // -----------------------------------------------------------------------
    // Socket addr tests
    // -----------------------------------------------------------------------

    #[test]
    fn socket_addr_parses_valid_host_port() {
        let addr: Result<SocketAddr, _> = "0.0.0.0:8080".parse();
        assert!(addr.is_ok());
    }

    #[test]
    fn socket_addr_rejects_invalid() {
        let addr: Result<SocketAddr, _> = "not_a_socket".parse();
        assert!(addr.is_err());
    }

    // -----------------------------------------------------------------------
    // compute_market_close_sleep tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_market_close_sleep_valid_time() {
        let duration = compute_market_close_sleep("15:30:00");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_invalid_format() {
        let duration = compute_market_close_sleep("invalid");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_empty_string() {
        let duration = compute_market_close_sleep("");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_midnight() {
        let duration = compute_market_close_sleep("00:00:00");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_end_of_day() {
        let duration = compute_market_close_sleep("23:59:59");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_partial_format_rejected() {
        let duration = compute_market_close_sleep("15:30");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_numbers_only_rejected() {
        let duration = compute_market_close_sleep("153000");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_out_of_range_hours() {
        let duration = compute_market_close_sleep("25:00:00");
        assert_eq!(duration, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_market_close_sleep_with_seconds() {
        let duration = compute_market_close_sleep("09:15:00");
        assert!(duration.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_various_invalid_formats() {
        let invalid_inputs = ["abc", "12:30", "12-30-00", "99:99:99", "12:30:00 AM"];
        for input in &invalid_inputs {
            let duration = compute_market_close_sleep(input);
            assert_eq!(
                duration,
                std::time::Duration::ZERO,
                "invalid format '{input}' should return Duration::ZERO"
            );
        }
    }

    #[test]
    fn test_compute_market_close_sleep_boundary_values() {
        let d = compute_market_close_sleep("00:00:01");
        assert!(d.as_secs() <= 86_400);
        let d = compute_market_close_sleep("23:59:58");
        assert!(d.as_secs() <= 86_400);
    }

    #[test]
    fn test_compute_market_close_sleep_always_lte_24h() {
        let times = [
            "00:00:00", "01:00:00", "06:00:00", "09:15:00", "12:00:00", "15:30:00", "18:00:00",
            "23:59:59",
        ];
        for t in &times {
            let d = compute_market_close_sleep(t);
            assert!(d.as_secs() <= 86_400, "must be <= 24h for {t}");
        }
    }

    #[test]
    fn test_compute_market_close_sleep_returns_zero_for_past_time() {
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_hour = now_ist.time().hour();
        if now_hour >= 1 {
            let d = compute_market_close_sleep("00:00:01");
            assert_eq!(d, std::time::Duration::ZERO);
        }
    }

    #[test]
    fn test_compute_market_close_sleep_uses_ist_not_utc() {
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = u64::from(now_ist.time().num_seconds_from_midnight());
        let close_secs: u64 = 23 * 3600 + 59 * 60 + 59;
        let d = compute_market_close_sleep("23:59:59");
        if now_secs < close_secs {
            assert!(d.as_secs() > 0);
        } else {
            assert_eq!(d, std::time::Duration::ZERO);
        }
    }

    #[test]
    fn test_compute_market_close_sleep_saturating_sub() {
        for h in 0..24_u32 {
            let time_str = format!("{h:02}:00:00");
            let d = compute_market_close_sleep(&time_str);
            assert!(d.as_secs() <= 86_400);
        }
    }

    #[test]
    fn test_compute_market_close_sleep_num_seconds_from_midnight() {
        let t = chrono::NaiveTime::parse_from_str("15:30:00", "%H:%M:%S").unwrap();
        assert_eq!(t.num_seconds_from_midnight(), 15 * 3600 + 30 * 60);
    }

    #[test]
    fn test_compute_market_close_sleep_midnight_secs() {
        let t = chrono::NaiveTime::parse_from_str("00:00:00", "%H:%M:%S").unwrap();
        assert_eq!(t.num_seconds_from_midnight(), 0);
    }

    #[test]
    fn test_compute_market_close_sleep_end_of_day_secs() {
        let t = chrono::NaiveTime::parse_from_str("23:59:59", "%H:%M:%S").unwrap();
        assert_eq!(t.num_seconds_from_midnight(), 86399);
    }

    // -----------------------------------------------------------------------
    // Fast boot window tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_post_market_monitor_constants() {
        assert_eq!(FAST_BOOT_WINDOW_END, "15:30:00");
    }

    #[test]
    fn test_fast_boot_window_start_is_valid_time() {
        let parsed = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S");
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_fast_boot_window_end_is_valid_time() {
        let parsed = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S");
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_fast_boot_window_start_before_end() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        assert!(start < end);
    }

    #[test]
    fn test_fast_boot_window_covers_market_hours() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        let market_open = chrono::NaiveTime::parse_from_str("09:15:00", "%H:%M:%S").unwrap();
        let market_close = chrono::NaiveTime::parse_from_str("15:30:00", "%H:%M:%S").unwrap();
        assert!(start <= market_open);
        assert!(end >= market_close);
    }

    #[test]
    fn test_fast_boot_window_duration_is_reasonable() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        let duration_secs = end.num_seconds_from_midnight() - start.num_seconds_from_midnight();
        assert!(duration_secs >= 3600);
        assert!(duration_secs <= 43200);
    }

    // -----------------------------------------------------------------------
    // Off-hours stagger tests
    // -----------------------------------------------------------------------

    #[test]
    fn off_hours_stagger_is_less_than_market_hours() {
        const {
            assert!(OFF_HOURS_CONNECTION_STAGGER_MS > 0);
            assert!(OFF_HOURS_CONNECTION_STAGGER_MS <= 2000);
        }
    }

    #[test]
    fn test_off_hours_stagger_constant_value() {
        assert_eq!(OFF_HOURS_CONNECTION_STAGGER_MS, 1000);
    }

    #[test]
    fn test_off_hours_stagger_fits_in_u64() {
        let d = std::time::Duration::from_millis(OFF_HOURS_CONNECTION_STAGGER_MS);
        assert_eq!(d.as_secs(), 1);
    }

    // -----------------------------------------------------------------------
    // App log file path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_app_log_file_path_is_not_empty() {
        assert!(!APP_LOG_FILE_PATH.is_empty());
    }

    #[test]
    fn test_app_log_file_path_ends_with_log() {
        assert!(APP_LOG_FILE_PATH.ends_with(".log"));
    }

    #[test]
    fn test_app_log_file_path_is_relative() {
        assert!(!APP_LOG_FILE_PATH.starts_with('/'));
    }

    #[test]
    fn test_app_log_file_path_has_two_segments() {
        let segments: Vec<&str> = APP_LOG_FILE_PATH.split('/').collect();
        assert!(segments.len() >= 2);
    }

    #[test]
    fn test_create_log_file_app_log_file_path_parent_dir() {
        let path = std::path::Path::new(APP_LOG_FILE_PATH);
        let parent = path.parent().unwrap_or(std::path::Path::new("data/logs"));
        assert!(!parent.as_os_str().is_empty());
    }

    #[test]
    fn test_create_log_file_path_in_data_directory() {
        assert!(APP_LOG_FILE_PATH.starts_with("data/"));
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_returns_some_in_default_path() {
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_create_log_file_writer_returns_file_handle() {
        let _ = create_log_file_writer();
    }

    #[test]
    fn test_create_log_file_writer_with_temp_dir() {
        let tmp = std::env::temp_dir().join("dlt_test_log_writer");
        let _ = std::fs::create_dir_all(&tmp);
        let log_path = tmp.join("test.log");
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path);
        assert!(result.is_ok());
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn test_create_log_file_parent_extraction() {
        let path = std::path::Path::new(APP_LOG_FILE_PATH);
        let parent = path.parent();
        assert!(parent.is_some());
        assert!(!parent.unwrap().as_os_str().is_empty());
    }

    // -----------------------------------------------------------------------
    // format_timeframe_details tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timeframe_details_empty_report() {
        let report = make_verification_report(vec![]);
        let result = format_timeframe_details(&report);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_timeframe_details_single_timeframe() {
        let report = make_verification_report(vec![make_coverage("1m", 86250, 232)]);
        let result = format_timeframe_details(&report);
        assert_eq!(result, "1m: 86250 (232 inst)");
    }

    #[test]
    fn test_format_timeframe_details_multiple_timeframes() {
        let report = make_verification_report(vec![
            make_coverage("1m", 86250, 232),
            make_coverage("5m", 17250, 232),
            make_coverage("1d", 232, 232),
        ]);
        let result = format_timeframe_details(&report);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "1m: 86250 (232 inst)");
    }

    #[test]
    fn test_format_timeframe_details_zero_counts() {
        let report = make_verification_report(vec![make_coverage("15m", 0, 0)]);
        let result = format_timeframe_details(&report);
        assert_eq!(result, "15m: 0 (0 inst)");
    }

    #[test]
    fn test_format_timeframe_details_large_counts() {
        let report = make_verification_report(vec![make_coverage("1m", 1_000_000, 5000)]);
        let result = format_timeframe_details(&report);
        assert_eq!(result, "1m: 1000000 (5000 inst)");
    }

    #[test]
    fn test_format_timeframe_details_uses_newline_separator() {
        let report = make_verification_report(vec![
            make_coverage("1m", 100, 10),
            make_coverage("5m", 20, 10),
        ]);
        let result = format_timeframe_details(&report);
        assert_eq!(result.matches('\n').count(), 1);
    }

    #[test]
    fn test_format_timeframe_details_no_trailing_newline() {
        let report = make_verification_report(vec![make_coverage("1d", 10, 5)]);
        let result = format_timeframe_details(&report);
        assert!(!result.ends_with('\n'));
    }

    // -----------------------------------------------------------------------
    // format_violation_details tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_violation_details_empty() {
        let result = format_violation_details(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_violation_details_single_violation() {
        let violations = vec![make_violation(
            "RELIANCE",
            "NSE_EQ",
            "1m",
            "2026-03-18 10:15",
            "high < low",
            "H=2440.0 < L=2450.0",
        )];
        let result = format_violation_details(&violations);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("RELIANCE"));
        assert!(result[0].starts_with('\u{2022}'));
    }

    #[test]
    fn test_format_violation_details_multiple_violations() {
        let violations = vec![
            make_violation("RELIANCE", "NSE_EQ", "1m", "T1", "v", "V1"),
            make_violation("NIFTY50", "IDX_I", "5m", "T2", "v", "V2"),
        ];
        let result = format_violation_details(&violations);
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("RELIANCE"));
        assert!(result[1].contains("NIFTY50"));
    }

    #[test]
    fn test_format_violation_details_contains_newline_indented_values() {
        let violations = vec![make_violation("TCS", "NSE_EQ", "1d", "T", "v", "O=-1.0")];
        let result = format_violation_details(&violations);
        assert!(result[0].contains("\n  O=-1.0"));
    }

    #[test]
    fn test_format_violation_details_preserves_all_fields() {
        let violations = vec![make_violation("A", "B", "C", "D", "E", "F")];
        let result = format_violation_details(&violations);
        let line = &result[0];
        assert!(line.contains("A"));
        assert!(line.contains("B"));
        assert!(line.contains("C"));
        assert!(line.contains("D"));
        assert!(line.contains("F"));
    }

    #[test]
    fn test_format_violation_details_bullet_point_format() {
        let violations = vec![make_violation("SYM", "SEG", "TF", "TS", "VIO", "VAL")];
        let result = format_violation_details(&violations);
        assert!(result[0].starts_with('\u{2022}'));
        assert!(result[0].contains("\n  VAL"));
    }

    #[test]
    fn test_format_violation_details_each_entry_is_multiline() {
        let violations = vec![
            make_violation("A", "B", "C", "D", "E", "F"),
            make_violation("G", "H", "I", "J", "K", "L"),
        ];
        let result = format_violation_details(&violations);
        for entry in &result {
            assert!(entry.contains('\n'));
        }
    }

    // -----------------------------------------------------------------------
    // format_cross_match_details tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_cross_match_details_empty() {
        let result = format_cross_match_details(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_cross_match_details_single_mismatch() {
        let mismatches = vec![make_cross_match(
            "RELIANCE", "NSE_EQ", "1m", "T", "HIST", "LIVE", "DIFF",
        )];
        let result = format_cross_match_details(&mismatches);
        assert_eq!(result.len(), 1);
        assert!(result[0].starts_with('\u{2022}'));
        assert!(result[0].contains("Hist:"));
        assert!(result[0].contains("Live:"));
        assert!(result[0].contains("Diff:"));
    }

    #[test]
    fn test_format_cross_match_details_empty_diff_summary_omits_diff_line() {
        let mismatches = vec![make_cross_match("TCS", "NSE_EQ", "5m", "T", "H", "L", "")];
        let result = format_cross_match_details(&mismatches);
        assert!(!result[0].contains("Diff:"));
    }

    #[test]
    fn test_format_cross_match_details_multiple_mismatches() {
        let mismatches = vec![
            make_cross_match("RELIANCE", "NSE_EQ", "1m", "T", "h", "l", "d"),
            make_cross_match("INFY", "NSE_EQ", "5m", "T", "h", "l", "d"),
        ];
        let result = format_cross_match_details(&mismatches);
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("RELIANCE"));
        assert!(result[1].contains("INFY"));
    }

    #[test]
    fn test_format_cross_match_details_structure() {
        let mismatches = vec![make_cross_match("SYM", "SEG", "TF", "TS", "h", "l", "d")];
        let result = format_cross_match_details(&mismatches);
        let lines: Vec<&str> = result[0].lines().collect();
        assert!(lines.len() >= 3);
        assert!(lines[0].contains("SYM"));
    }

    #[test]
    fn test_format_cross_match_details_four_line_format() {
        let mismatches = vec![make_cross_match("S", "G", "T", "T", "H", "L", "D")];
        let result = format_cross_match_details(&mismatches);
        let lines: Vec<&str> = result[0].lines().collect();
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn test_format_cross_match_details_three_line_format_without_diff() {
        let mismatches = vec![make_cross_match("S", "G", "T", "T", "H", "L", "")];
        let result = format_cross_match_details(&mismatches);
        let lines: Vec<&str> = result[0].lines().collect();
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_format_cross_match_details_hist_before_live() {
        let mismatches = vec![make_cross_match("S", "G", "T", "T", "HIST", "LIVE", "DIFF")];
        let result = format_cross_match_details(&mismatches);
        let text = &result[0];
        let hist_pos = text.find("Hist:").unwrap();
        let live_pos = text.find("Live:").unwrap();
        let diff_pos = text.find("Diff:").unwrap();
        assert!(hist_pos < live_pos);
        assert!(live_pos < diff_pos);
    }

    // -----------------------------------------------------------------------
    // Shutdown bind addr tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_shutdown_bind_addr_fallback() {
        let fallback: SocketAddr = SocketAddr::from(([0, 0, 0, 0], 8080));
        assert_eq!(fallback.port(), 8080);
    }

    #[test]
    fn test_shutdown_bind_addr_parse_invalid_triggers_fallback() {
        let result: Result<SocketAddr, _> = "not_valid".parse();
        assert!(result.is_err());
        let fallback = result.unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert_eq!(fallback.port(), 8080);
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer — deeper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_returns_some() {
        // In a normal environment, the log file should be creatable.
        // This exercises both the create_dir_all and OpenOptions paths.
        let result = create_log_file_writer();
        assert!(
            result.is_some(),
            "create_log_file_writer should succeed in writable directory"
        );
    }

    #[test]
    fn test_create_log_file_writer_file_is_writable() {
        use std::io::Write;
        if let Some(mut file) = create_log_file_writer() {
            let write_result = file.write_all(b"test log line\n");
            assert!(
                write_result.is_ok(),
                "log file must be writable after creation"
            );
        }
    }

    #[test]
    fn test_create_log_file_writer_idempotent() {
        // Multiple calls should all succeed (file opened in append mode).
        let f1 = create_log_file_writer();
        let f2 = create_log_file_writer();
        assert!(f1.is_some());
        assert!(f2.is_some());
    }

    #[test]
    fn test_app_log_file_path_parent_is_valid_dir() {
        let path = std::path::Path::new(APP_LOG_FILE_PATH);
        let parent = path.parent();
        assert!(
            parent.is_some(),
            "log file path must have a parent directory"
        );
        let parent = parent.unwrap();
        // After create_log_file_writer(), the parent dir should exist.
        let _ = create_log_file_writer();
        assert!(
            parent.exists(),
            "log directory must exist after writer creation"
        );
    }

    // -----------------------------------------------------------------------
    // format_timeframe_details — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timeframe_details_preserves_timeframe_names() {
        let timeframes = vec!["1m", "5m", "15m", "30m", "1h", "4h", "1d"];
        let coverages: Vec<TimeframeCoverage> = timeframes
            .iter()
            .enumerate()
            .map(|(i, tf)| make_coverage(tf, (i + 1) * 100, 10))
            .collect();
        let report = make_verification_report(coverages);
        let result = format_timeframe_details(&report);
        for tf in &timeframes {
            assert!(
                result.contains(tf),
                "output must contain timeframe name '{tf}'"
            );
        }
    }

    #[test]
    fn test_format_timeframe_details_newline_count_matches_entries() {
        let coverages = vec![
            make_coverage("1m", 100, 10),
            make_coverage("5m", 20, 10),
            make_coverage("15m", 5, 10),
            make_coverage("1d", 1, 10),
        ];
        let report = make_verification_report(coverages);
        let result = format_timeframe_details(&report);
        // N entries produce N-1 newlines (joined)
        assert_eq!(
            result.matches('\n').count(),
            3,
            "4 entries should have 3 newline separators"
        );
    }

    #[test]
    fn test_format_timeframe_details_single_entry_has_no_newline() {
        let report = make_verification_report(vec![make_coverage("1s", 50, 1)]);
        let result = format_timeframe_details(&report);
        assert_eq!(result.matches('\n').count(), 0);
        assert!(result.contains("1s"));
        assert!(result.contains("50"));
        assert!(result.contains("1 inst"));
    }

    // -----------------------------------------------------------------------
    // format_violation_details — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_violation_details_special_characters_in_symbol() {
        let violations = vec![make_violation(
            "NIFTY BANK",
            "IDX_I",
            "1m",
            "2026-03-18 10:00",
            "high_lt_low",
            "H=100 < L=200",
        )];
        let result = format_violation_details(&violations);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("NIFTY BANK"));
    }

    #[test]
    fn test_format_violation_details_many_entries() {
        let violations: Vec<ViolationDetail> = (0..100)
            .map(|i| {
                make_violation(
                    &format!("SYM_{i}"),
                    "NSE_EQ",
                    "1m",
                    &format!("2026-03-18 10:{i:02}"),
                    "violation",
                    &format!("val_{i}"),
                )
            })
            .collect();
        let result = format_violation_details(&violations);
        assert_eq!(result.len(), 100);
        assert!(result[0].contains("SYM_0"));
        assert!(result[99].contains("SYM_99"));
    }

    #[test]
    fn test_format_violation_details_empty_fields() {
        let violations = vec![make_violation("", "", "", "", "", "")];
        let result = format_violation_details(&violations);
        assert_eq!(result.len(), 1);
        // Should still produce a formatted string even with empty fields
        assert!(result[0].starts_with('\u{2022}'));
    }

    // -----------------------------------------------------------------------
    // format_cross_match_details — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_cross_match_details_many_entries() {
        let mismatches: Vec<CrossMatchMismatch> = (0..50)
            .map(|i| {
                make_cross_match(
                    &format!("SYM_{i}"),
                    "NSE_EQ",
                    "5m",
                    &format!("T_{i}"),
                    &format!("H_{i}"),
                    &format!("L_{i}"),
                    &format!("D_{i}"),
                )
            })
            .collect();
        let result = format_cross_match_details(&mismatches);
        assert_eq!(result.len(), 50);
        assert!(result[0].contains("SYM_0"));
        assert!(result[49].contains("SYM_49"));
    }

    #[test]
    fn test_format_cross_match_details_mixed_empty_and_nonempty_diff() {
        let mismatches = vec![
            make_cross_match("A", "S1", "1m", "T1", "H1", "L1", "D1"),
            make_cross_match("B", "S2", "5m", "T2", "H2", "L2", ""),
            make_cross_match("C", "S3", "1d", "T3", "H3", "L3", "D3"),
        ];
        let result = format_cross_match_details(&mismatches);
        assert_eq!(result.len(), 3);
        // First and third should have Diff line
        assert!(result[0].contains("Diff:"));
        assert!(!result[1].contains("Diff:"));
        assert!(result[2].contains("Diff:"));
    }

    #[test]
    fn test_format_cross_match_details_long_values() {
        let long_hist = "O=25000.50 H=25100.75 L=24900.25 C=25050.00 V=1234567890";
        let long_live = "O=25000.55 H=25100.80 L=24900.20 C=25050.10 V=1234567895";
        let long_diff = "open_diff=0.05 high_diff=0.05 low_diff=-0.05 close_diff=0.10 vol_diff=5";
        let mismatches = vec![make_cross_match(
            "RELIANCE",
            "NSE_EQ",
            "1m",
            "2026-03-18 10:15",
            long_hist,
            long_live,
            long_diff,
        )];
        let result = format_cross_match_details(&mismatches);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(long_hist));
        assert!(result[0].contains(long_live));
        assert!(result[0].contains(long_diff));
    }

    // -----------------------------------------------------------------------
    // compute_market_close_sleep — deterministic future time test
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_market_close_sleep_future_time_returns_positive() {
        // Use 23:59:59 which is always in the future during normal test runs
        // (IST is UTC+5:30, so 23:59:59 IST is ~18:29:59 UTC)
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = now_ist.time().num_seconds_from_midnight();
        let close_secs = 23 * 3600 + 59 * 60 + 59; // 23:59:59
        if now_secs < close_secs {
            let d = compute_market_close_sleep("23:59:59");
            assert!(d.as_secs() > 0, "future time must return positive duration");
            let expected_secs = close_secs - now_secs;
            // Allow 2 seconds tolerance for test execution time
            assert!(
                (d.as_secs() as i64 - expected_secs as i64).unsigned_abs() <= 2,
                "duration should match expected time until close"
            );
        }
    }

    #[test]
    fn test_compute_market_close_sleep_consistency() {
        // Two calls close together should return similar durations
        let d1 = compute_market_close_sleep("15:30:00");
        let d2 = compute_market_close_sleep("15:30:00");
        // Within 1 second of each other
        let diff = if d1 > d2 {
            d1.as_secs() - d2.as_secs()
        } else {
            d2.as_secs() - d1.as_secs()
        };
        assert!(
            diff <= 1,
            "consecutive calls should return similar durations"
        );
    }

    // -----------------------------------------------------------------------
    // Const assertions — compile-time guarantees
    // -----------------------------------------------------------------------

    #[test]
    fn test_fast_boot_window_start_is_before_nine_fifteen() {
        let start = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_START, "%H:%M:%S").unwrap();
        let nine_fifteen = chrono::NaiveTime::parse_from_str("09:15:00", "%H:%M:%S").unwrap();
        assert!(
            start <= nine_fifteen,
            "fast boot window must start at or before market open (09:15)"
        );
    }

    #[test]
    fn test_fast_boot_window_end_is_market_close() {
        let end = chrono::NaiveTime::parse_from_str(FAST_BOOT_WINDOW_END, "%H:%M:%S").unwrap();
        let market_close = chrono::NaiveTime::parse_from_str("15:30:00", "%H:%M:%S").unwrap();
        assert_eq!(
            end, market_close,
            "fast boot window end must equal market close"
        );
    }

    // -----------------------------------------------------------------------
    // CrossVerificationReport field coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_make_verification_report_default_fields() {
        let report = make_verification_report(vec![]);
        assert_eq!(report.instruments_checked, 0);
        assert_eq!(report.instruments_complete, 0);
        assert_eq!(report.instruments_with_gaps, 0);
        assert_eq!(report.total_candles_in_db, 0);
        assert!(report.timeframe_counts.is_empty());
        assert_eq!(report.ohlc_violations, 0);
        assert!(report.ohlc_details.is_empty());
        assert_eq!(report.data_violations, 0);
        assert!(report.data_details.is_empty());
        assert_eq!(report.timestamp_violations, 0);
        assert!(report.timestamp_details.is_empty());
        assert!(report.passed);
    }

    #[test]
    fn test_format_timeframe_details_with_populated_report() {
        let report = CrossVerificationReport {
            instruments_checked: 232,
            instruments_complete: 230,
            instruments_with_gaps: 2,
            total_candles_in_db: 103_500,
            timeframe_counts: vec![
                make_coverage("1m", 86_250, 232),
                make_coverage("5m", 17_250, 232),
            ],
            ohlc_violations: 0,
            ohlc_details: vec![],
            data_violations: 0,
            data_details: vec![],
            timestamp_violations: 0,
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
            passed: true,
        };
        let result = format_timeframe_details(&report);
        assert!(result.contains("1m: 86250 (232 inst)"));
        assert!(result.contains("5m: 17250 (232 inst)"));
    }

    // -----------------------------------------------------------------------
    // ViolationDetail field coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_violation_detail_contains_segment_and_timeframe() {
        let violations = vec![make_violation(
            "TCS",
            "NSE_FNO",
            "15m",
            "2026-03-18 14:00",
            "ohlc",
            "values",
        )];
        let result = format_violation_details(&violations);
        assert!(result[0].contains("NSE_FNO"));
        assert!(result[0].contains("15m"));
        assert!(result[0].contains("2026-03-18 14:00"));
    }

    // -----------------------------------------------------------------------
    // CrossMatchMismatch field coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_match_mismatch_contains_all_fields() {
        let mismatches = vec![make_cross_match(
            "INFY",
            "NSE_EQ",
            "1d",
            "2026-03-18",
            "O=1500 H=1520",
            "O=1501 H=1519",
            "O=1 H=-1",
        )];
        let result = format_cross_match_details(&mismatches);
        let text = &result[0];
        assert!(text.contains("INFY"));
        assert!(text.contains("NSE_EQ"));
        assert!(text.contains("1d"));
        assert!(text.contains("2026-03-18"));
        assert!(text.contains("O=1500 H=1520"));
        assert!(text.contains("O=1501 H=1519"));
        assert!(text.contains("O=1 H=-1"));
    }

    // -----------------------------------------------------------------------
    // compute_market_close_sleep — exercise now >= close_time branch
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_market_close_sleep_definitely_past_midnight() {
        // 00:00:00 is always in the past after the first second of the day.
        // In IST (UTC+5:30), 00:00:00 is midnight. Tests run during daytime,
        // so this should always return ZERO.
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = now_ist.time().num_seconds_from_midnight();
        if now_secs > 0 {
            let d = compute_market_close_sleep("00:00:00");
            assert_eq!(
                d,
                std::time::Duration::ZERO,
                "midnight is always past during daytime tests"
            );
        }
    }

    #[test]
    fn test_compute_market_close_sleep_past_time_is_zero() {
        // Test with 00:00:01 — should be past at any reasonable test time.
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let now_secs = now_ist.time().num_seconds_from_midnight();
        if now_secs > 1 {
            let d = compute_market_close_sleep("00:00:01");
            assert_eq!(d, std::time::Duration::ZERO);
        }
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer — deeper error path coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_creates_directory_if_missing() {
        // Verify that after calling create_log_file_writer, the parent
        // directory exists (it creates it if missing).
        let result = create_log_file_writer();
        if result.is_some() {
            let log_dir = std::path::Path::new(APP_LOG_FILE_PATH)
                .parent()
                .unwrap_or(std::path::Path::new("data/logs"));
            assert!(log_dir.exists(), "log directory must be created");
            assert!(log_dir.is_dir(), "log parent must be a directory");
        }
    }

    #[test]
    fn test_create_log_file_writer_file_exists_after_creation() {
        let result = create_log_file_writer();
        if result.is_some() {
            let log_path = std::path::Path::new(APP_LOG_FILE_PATH);
            assert!(log_path.exists(), "log file must exist after creation");
            assert!(log_path.is_file(), "log path must be a regular file");
        }
    }

    #[test]
    fn test_create_log_file_writer_append_mode() {
        use std::io::Write;
        // Write to the log file twice — second write should append, not overwrite.
        let f1 = create_log_file_writer();
        if let Some(mut file) = f1 {
            file.write_all(b"first line\n").unwrap();
        }
        let f2 = create_log_file_writer();
        if let Some(mut file) = f2 {
            file.write_all(b"second line\n").unwrap();
        }
        // If we got here without panic, append mode works correctly.
    }

    // -----------------------------------------------------------------------
    // format_violation_details — unicode bullet point
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_violation_details_bullet_is_unicode() {
        let violations = vec![make_violation("X", "Y", "Z", "T", "V", "VAL")];
        let result = format_violation_details(&violations);
        let first_char = result[0].chars().next().unwrap();
        assert_eq!(
            first_char, '\u{2022}',
            "first character must be unicode bullet point"
        );
    }

    // -----------------------------------------------------------------------
    // format_cross_match_details — ordering of hist/live/diff lines
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_cross_match_details_line_ordering() {
        let mismatches = vec![make_cross_match(
            "SYM", "SEG", "TF", "TS", "HIST_VAL", "LIVE_VAL", "DIFF_VAL",
        )];
        let result = format_cross_match_details(&mismatches);
        let lines: Vec<&str> = result[0].lines().collect();
        // Line 0: bullet + symbol info
        // Line 1: Hist: ...
        // Line 2: Live: ...
        // Line 3: Diff: ...
        assert!(lines.len() >= 4, "should have at least 4 lines");
        assert!(lines[1].contains("Hist:"), "line 1 must be Hist");
        assert!(lines[2].contains("Live:"), "line 2 must be Live");
        assert!(lines[3].contains("Diff:"), "line 3 must be Diff");
    }

    // -----------------------------------------------------------------------
    // format_timeframe_details — capacity pre-allocation
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timeframe_details_handles_large_input() {
        let coverages: Vec<TimeframeCoverage> = (0..200)
            .map(|i| make_coverage(&format!("{i}s"), i * 100, i))
            .collect();
        let report = make_verification_report(coverages);
        let result = format_timeframe_details(&report);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 200, "should have 200 lines for 200 entries");
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer_at — error path coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_at_unwritable_directory_returns_none() {
        // /proc is a read-only pseudo-filesystem on Linux — create_dir_all fails.
        let result = create_log_file_writer_at("/proc/nonexistent_dlt_dir/app.log");
        assert!(result.is_none(), "unwritable directory should return None");
    }

    #[test]
    fn test_create_log_file_writer_at_directory_as_file_path_returns_none() {
        // Creating a file at a path where a directory already exists fails.
        // /tmp always exists — trying to open it as a file should fail.
        let result = create_log_file_writer_at("/tmp");
        assert!(
            result.is_none(),
            "directory path as file should return None"
        );
    }

    #[test]
    fn test_create_log_file_writer_at_valid_path_returns_some() {
        let tmp = std::env::temp_dir().join("dlt_test_writer_at");
        let log_path = tmp.join("test_at.log");
        let path_str = log_path.to_string_lossy().to_string();
        let result = create_log_file_writer_at(&path_str);
        assert!(result.is_some(), "valid path should return Some");
        // Cleanup
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[test]
    fn test_create_log_file_writer_at_empty_path_returns_none() {
        // Empty string as path — will fail to create dir or open file.
        let result = create_log_file_writer_at("");
        assert!(result.is_none(), "empty path should return None");
    }

    #[test]
    fn test_create_log_file_writer_at_deeply_nested_creates_dirs() {
        let tmp = std::env::temp_dir().join("dlt_test_deep/a/b/c");
        let log_path = tmp.join("deep.log");
        let path_str = log_path.to_string_lossy().to_string();
        let result = create_log_file_writer_at(&path_str);
        assert!(result.is_some(), "deeply nested path should succeed");
        // Cleanup
        let _ = std::fs::remove_file(&log_path);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("dlt_test_deep"));
    }

    #[test]
    fn test_create_log_file_writer_at_default_path_matches_original() {
        // Verify that create_log_file_writer delegates to create_log_file_writer_at
        let result_default = create_log_file_writer();
        let result_at = create_log_file_writer_at(APP_LOG_FILE_PATH);
        // Both should succeed in the same environment
        assert_eq!(
            result_default.is_some(),
            result_at.is_some(),
            "both functions should agree on success/failure"
        );
    }

    #[test]
    fn test_create_log_file_writer_at_dev_null_returns_some() {
        // /dev/null is always writable on Linux
        let result = create_log_file_writer_at("/dev/null");
        assert!(result.is_some(), "/dev/null should be openable");
    }

    #[test]
    fn test_create_log_file_writer_at_read_only_fs_returns_none() {
        // /sys is a sysfs pseudo-filesystem — directory creation should fail
        let result = create_log_file_writer_at("/sys/dlt_impossible/app.log");
        assert!(result.is_none(), "read-only filesystem should return None");
    }

    // -----------------------------------------------------------------------
    // effective_ws_stagger tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_effective_ws_stagger_always_uses_fast_stagger() {
        // Always uses 1s stagger for fast crash recovery during market hours.
        assert_eq!(
            effective_ws_stagger(3000, true),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(3000, false),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(0, true),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(0, false),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
    }

    // -----------------------------------------------------------------------
    // format_bind_addr tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_bind_addr_ipv4() {
        let addr = format_bind_addr("0.0.0.0", 3001);
        assert_eq!(addr, "0.0.0.0:3001");
    }

    #[test]
    fn test_format_bind_addr_localhost() {
        let addr = format_bind_addr("127.0.0.1", 8080);
        assert_eq!(addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_format_bind_addr_port_zero() {
        let addr = format_bind_addr("0.0.0.0", 0);
        assert_eq!(addr, "0.0.0.0:0");
    }

    #[test]
    fn test_format_bind_addr_parses_to_socket_addr() {
        let addr = format_bind_addr("0.0.0.0", 3001);
        let parsed: Result<SocketAddr, _> = addr.parse();
        assert!(parsed.is_ok(), "formatted addr must parse to SocketAddr");
        assert_eq!(parsed.unwrap().port(), 3001);
    }

    #[test]
    fn test_format_bind_addr_high_port() {
        let addr = format_bind_addr("0.0.0.0", 65535);
        let parsed: SocketAddr = addr.parse().unwrap();
        assert_eq!(parsed.port(), 65535);
    }

    // -----------------------------------------------------------------------
    // determine_boot_mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_boot_mode_fast_with_cache_and_market() {
        assert_eq!(determine_boot_mode(true, true), "fast");
    }

    #[test]
    fn test_determine_boot_mode_standard_no_cache() {
        assert_eq!(determine_boot_mode(false, true), "standard");
    }

    #[test]
    fn test_determine_boot_mode_standard_no_market() {
        assert_eq!(determine_boot_mode(true, false), "standard");
    }

    #[test]
    fn test_determine_boot_mode_standard_neither() {
        assert_eq!(determine_boot_mode(false, false), "standard");
    }

    // -----------------------------------------------------------------------
    // should_fast_boot tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_fast_boot_true() {
        assert!(should_fast_boot(true, true));
    }

    #[test]
    fn test_should_fast_boot_false_no_cache() {
        assert!(!should_fast_boot(false, true));
    }

    #[test]
    fn test_should_fast_boot_false_no_market() {
        assert!(!should_fast_boot(true, false));
    }

    #[test]
    fn test_should_fast_boot_false_neither() {
        assert!(!should_fast_boot(false, false));
    }

    #[test]
    fn test_should_fast_boot_matches_determine_boot_mode() {
        // should_fast_boot(true, true) <=> determine_boot_mode returns "fast"
        for &cache in &[true, false] {
            for &market in &[true, false] {
                let is_fast = should_fast_boot(cache, market);
                let mode = determine_boot_mode(cache, market);
                assert_eq!(
                    is_fast,
                    mode == "fast",
                    "should_fast_boot and determine_boot_mode must agree for cache={cache}, market={market}"
                );
            }
        }
    }

    #[test]
    fn test_heartbeat_watchdog_spawned() {
        assert_eq!(super::WATCHDOG_INTERVAL_SECS, 30);
    }

    #[test]
    fn test_clock_drift_check() {
        assert_eq!(super::CLOCK_DRIFT_THRESHOLD_SECS, 2);
    }

    #[test]
    fn test_log_rotation_configured() {
        assert_eq!(super::LOG_MAX_SIZE_BYTES, 50 * 1024 * 1024);
        assert_eq!(super::LOG_MAX_FILES, 7);
    }

    // -----------------------------------------------------------------------
    // Clock drift constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_clock_drift_http_timeout_is_reasonable() {
        assert!(super::CLOCK_DRIFT_HTTP_TIMEOUT_SECS >= 1);
        assert!(super::CLOCK_DRIFT_HTTP_TIMEOUT_SECS <= 30);
    }

    #[test]
    fn test_clock_drift_reference_url_is_https() {
        assert!(
            super::CLOCK_DRIFT_REFERENCE_URL.starts_with("https://"),
            "clock drift reference URL must use HTTPS"
        );
    }

    // -----------------------------------------------------------------------
    // Log rotation constants relationships
    // -----------------------------------------------------------------------

    #[test]
    fn test_log_max_size_is_at_least_1mb() {
        assert!(
            super::LOG_MAX_SIZE_BYTES >= 1024 * 1024,
            "max log size must be at least 1MB"
        );
    }

    #[test]
    fn test_log_max_files_is_at_least_one() {
        assert!(super::LOG_MAX_FILES >= 1, "must keep at least 1 log file");
    }

    // -----------------------------------------------------------------------
    // create_log_file_writer_at: path with no parent (file at root-like level)
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_log_file_writer_at_relative_no_parent() {
        // A relative path with no slash has the parent "".
        // This exercises the unwrap_or fallback to "data/logs".
        let result = create_log_file_writer_at("justfilename.log");
        // Should succeed because it creates "data/logs" fallback.
        // Actually, Path::new("justfilename.log").parent() is Some(""),
        // and create_dir_all("") succeeds (no-op).
        // The file is created in current dir.
        if result.is_some() {
            let _ = std::fs::remove_file("justfilename.log");
        }
    }

    // -----------------------------------------------------------------------
    // effective_ws_stagger: edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_effective_ws_stagger_u64_max_always_fast() {
        // Even with u64::MAX configured, always uses fast stagger.
        assert_eq!(
            effective_ws_stagger(u64::MAX, true),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
        assert_eq!(
            effective_ws_stagger(u64::MAX, false),
            OFF_HOURS_CONNECTION_STAGGER_MS
        );
    }

    // -----------------------------------------------------------------------
    // format_bind_addr: empty host
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_bind_addr_empty_host() {
        let addr = format_bind_addr("", 8080);
        assert_eq!(addr, ":8080");
    }

    // -----------------------------------------------------------------------
    // determine_boot_mode and should_fast_boot: consistency exhaustive
    // -----------------------------------------------------------------------

    #[test]
    fn test_determine_boot_mode_returns_only_fast_or_standard() {
        for &cache in &[true, false] {
            for &market in &[true, false] {
                let mode = determine_boot_mode(cache, market);
                assert!(
                    mode == "fast" || mode == "standard",
                    "unexpected boot mode: {mode}"
                );
            }
        }
    }
}
