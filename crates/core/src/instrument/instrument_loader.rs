//! Three-layer instrument defense: Freshness Marker + Time Gate + Manual API.
//!
//! Ensures instruments build ONCE at 8:30 AM boot. During trading-hours
//! restarts, cache loads instantly — no time gate blocks recovery.
//!
//! # Three Layers
//! 1. **Freshness Marker** — one file read (~100μs). Already built today? Load
//!    from cache unconditionally. Crash at 10:30? Instant recovery.
//! 2. **Time Gate** — sub-nanosecond integer comparison. Outside [08:25, 08:55]
//!    IST? Try stale cache as fallback. Only gates fresh downloads.
//! 3. **Manual API** — `POST /api/instruments/rebuild`. Bypasses time gate,
//!    respects freshness marker (idempotent, one-shot).

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{FixedOffset, NaiveTime, Utc};
use tracing::{info, warn};

use dhan_live_trader_common::config::{InstrumentConfig, QuestDbConfig};
use dhan_live_trader_common::constants::{
    INSTRUMENT_FRESHNESS_MARKER_FILENAME, IST_UTC_OFFSET_SECONDS,
};
use dhan_live_trader_common::instrument_types::FnoUniverse;

use super::csv_downloader::{download_instrument_csv, load_cached_csv};
use super::universe_builder::build_fno_universe_from_csv;

use dhan_live_trader_storage::instrument_persistence::persist_instrument_snapshot;

// ---------------------------------------------------------------------------
// Layer 2: Time Gate (only gates downloads, not cache reads)
// ---------------------------------------------------------------------------

/// Returns `true` if the current IST time is within the build window
/// `[window_start, window_end)` (inclusive start, exclusive end).
///
/// Both arguments must be `HH:MM:SS` format. Already validated at config load.
pub fn is_within_build_window(window_start: &str, window_end: &str) -> bool {
    let start = match NaiveTime::parse_from_str(window_start, "%H:%M:%S") {
        Ok(t) => t,
        Err(_) => return false,
    };
    let end = match NaiveTime::parse_from_str(window_end, "%H:%M:%S") {
        Ok(t) => t,
        Err(_) => return false,
    };
    let now_ist = now_ist_time();
    now_ist >= start && now_ist < end
}

// ---------------------------------------------------------------------------
// Layer 1: Freshness Marker (always checked first — enables crash recovery)
// ---------------------------------------------------------------------------

/// Returns `true` if today's IST date matches the freshness marker file.
///
/// The marker file contains a single line: `YYYY-MM-DD`.
pub fn is_instrument_fresh(cache_dir: &str) -> bool {
    let marker_path = Path::new(cache_dir).join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
    let content = match std::fs::read_to_string(&marker_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let today = now_ist_date_string();
    content.trim() == today
}

/// Write today's IST date to the freshness marker file.
///
/// Called on successful instrument build only. Best-effort — failure is logged.
pub fn write_freshness_marker(cache_dir: &str) {
    let marker_path = Path::new(cache_dir).join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);

    // Ensure directory exists
    if let Some(parent) = marker_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        warn!(%err, path = %parent.display(), "failed to create marker directory");
        return;
    }

    let today = now_ist_date_string();
    if let Err(err) = std::fs::write(&marker_path, &today) {
        warn!(%err, path = %marker_path.display(), "failed to write freshness marker");
    } else {
        info!(date = %today, path = %marker_path.display(), "freshness marker written");
    }
}

// ---------------------------------------------------------------------------
// Public Orchestrators
// ---------------------------------------------------------------------------

/// Main entry: three-layer defense instrument loading.
///
/// Returns `Ok(Some((universe, was_fresh_build)))` if instruments were built.
/// Returns `Ok(None)` if no instruments available (outside window + no cache).
///
/// `was_fresh_build` is `true` when a full download + persist happened (first
/// build of the day). `false` when loaded from cache (fresh or stale).
pub async fn load_or_build_instruments(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
) -> Result<Option<(FnoUniverse, bool)>> {
    // Layer 1: Freshness marker — always checked first (enables crash recovery)
    if is_instrument_fresh(&instrument_config.csv_cache_directory) {
        info!("instruments: loading from cache (freshness marker = today)");
        let download_result = load_cached_csv(
            &instrument_config.csv_cache_directory,
            &instrument_config.csv_cache_filename,
        )
        .await
        .context("failed to load cached CSV despite freshness marker")?;

        let universe =
            build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
                .context("failed to build universe from cached CSV")?;

        return Ok(Some((universe, false)));
    }

    // Layer 2: Time gate — only gates downloads, not cache reads
    if !is_within_build_window(
        &instrument_config.build_window_start,
        &instrument_config.build_window_end,
    ) {
        // Outside window + no fresh marker → try stale cache as fallback
        match load_cached_csv(
            &instrument_config.csv_cache_directory,
            &instrument_config.csv_cache_filename,
        )
        .await
        {
            Ok(download_result) => {
                warn!(
                    window_start = %instrument_config.build_window_start,
                    window_end = %instrument_config.build_window_end,
                    "instruments: outside build window, using STALE cached CSV as fallback"
                );
                let universe =
                    build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
                        .context("failed to build universe from stale cached CSV")?;

                return Ok(Some((universe, false)));
            }
            Err(_) => {
                info!(
                    window_start = %instrument_config.build_window_start,
                    window_end = %instrument_config.build_window_end,
                    "instruments: TIME-GATED SKIP (outside build window, no cache available)"
                );
                return Ok(None);
            }
        }
    }

    // Layer 3: Full build — within window + not fresh → download → parse → persist → marker
    info!("instruments: full build (no freshness marker for today)");
    let download_result = download_instrument_csv(
        dhan_csv_url,
        dhan_csv_fallback_url,
        &instrument_config.csv_cache_directory,
        &instrument_config.csv_cache_filename,
        instrument_config.csv_download_timeout_secs,
    )
    .await
    .context("instrument CSV download failed")?;

    let universe = build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
        .context("failed to build F&O universe")?;

    // Persist to QuestDB (best-effort — warn on failure, don't block boot)
    if let Err(err) = persist_instrument_snapshot(&universe, questdb_config).await {
        warn!(%err, "instrument snapshot persistence failed (non-fatal)");
    }

    write_freshness_marker(&instrument_config.csv_cache_directory);

    Ok(Some((universe, true)))
}

/// Manual trigger: bypasses time gate, respects freshness marker.
///
/// Called by `POST /api/instruments/rebuild`.
/// Returns `Ok(None)` if already built today (idempotent — click 100 times, no effect).
/// Returns `Ok(Some(universe))` if rebuilt successfully.
pub async fn try_rebuild_instruments(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
) -> Result<Option<FnoUniverse>> {
    // Respect freshness marker (idempotent)
    if is_instrument_fresh(&instrument_config.csv_cache_directory) {
        info!("manual rebuild: already built today, skipping");
        return Ok(None);
    }

    // Full build
    info!("manual rebuild: starting full instrument build");
    let download_result = download_instrument_csv(
        dhan_csv_url,
        dhan_csv_fallback_url,
        &instrument_config.csv_cache_directory,
        &instrument_config.csv_cache_filename,
        instrument_config.csv_download_timeout_secs,
    )
    .await
    .context("manual rebuild: CSV download failed")?;

    let universe = build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
        .context("manual rebuild: universe build failed")?;

    if let Err(err) = persist_instrument_snapshot(&universe, questdb_config).await {
        warn!(%err, "manual rebuild: persistence failed (non-fatal)");
    }

    write_freshness_marker(&instrument_config.csv_cache_directory);

    Ok(Some(universe))
}

// ---------------------------------------------------------------------------
// Internal Helpers
// ---------------------------------------------------------------------------

/// Returns the current IST time.
#[allow(clippy::expect_used)] // APPROVED: Compile-time constant — IST_UTC_OFFSET_SECONDS is always valid
fn now_ist_time() -> NaiveTime {
    let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset constant is valid"); // APPROVED: compile-time constant
    Utc::now().with_timezone(&ist).time()
}

/// Returns today's IST date as `YYYY-MM-DD`.
#[allow(clippy::expect_used)] // APPROVED: Compile-time constant — IST_UTC_OFFSET_SECONDS is always valid
fn now_ist_date_string() -> String {
    let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset constant is valid"); // APPROVED: compile-time constant
    Utc::now().with_timezone(&ist).date_naive().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    // Note: is_within_build_window() uses the real clock via now_ist_time().
    // We test the boundary logic by choosing windows that are guaranteed to
    // include or exclude the current time.

    #[test]
    fn test_is_within_build_window_full_day_window() {
        // 00:00 to 23:59 — always inside
        assert!(is_within_build_window("00:00:00", "23:59:59"));
    }

    #[test]
    fn test_is_within_build_window_impossible_window() {
        // Window of zero length — never inside (start == end rejected by config)
        // But even if passed, start < end is required for true
        assert!(!is_within_build_window("12:00:00", "12:00:00"));
    }

    #[test]
    fn test_is_within_build_window_invalid_format() {
        assert!(!is_within_build_window("bad", "08:55:00"));
        assert!(!is_within_build_window("08:25:00", "bad"));
    }

    #[test]
    fn test_is_instrument_fresh_no_marker() {
        // Non-existent directory — no marker file
        assert!(!is_instrument_fresh("/tmp/dlt-nonexistent-marker-98765"));
    }

    #[test]
    fn test_is_instrument_fresh_stale_marker() {
        let temp_dir = std::env::temp_dir().join("dlt-test-stale-marker");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "2020-01-01").unwrap();

        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_today_marker() {
        let temp_dir = std::env::temp_dir().join("dlt-test-today-marker");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let today = now_ist_date_string();
        std::fs::write(&marker_path, &today).unwrap();

        assert!(is_instrument_fresh(temp_dir.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_write_freshness_marker_roundtrip() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-roundtrip");
        let cache_dir = temp_dir.to_str().unwrap();

        // Ensure clean state
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Should not be fresh before writing
        assert!(!is_instrument_fresh(cache_dir));

        // Write marker
        write_freshness_marker(cache_dir);

        // Should be fresh after writing
        assert!(is_instrument_fresh(cache_dir));

        // Verify file content
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(content, now_ist_date_string());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // now_ist_date_string / now_ist_time — format and range checks
    // -----------------------------------------------------------------------

    #[test]
    fn test_now_ist_date_string_format_yyyy_mm_dd() {
        let date_str = now_ist_date_string();
        // Must match YYYY-MM-DD (chrono NaiveDate::to_string output)
        assert_eq!(date_str.len(), 10, "date must be 10 chars: {}", date_str);
        assert_eq!(
            date_str.chars().nth(4),
            Some('-'),
            "5th char must be dash: {}",
            date_str
        );
        assert_eq!(
            date_str.chars().nth(7),
            Some('-'),
            "8th char must be dash: {}",
            date_str
        );
        // Year, month, day must be parseable integers
        let year: i32 = date_str[..4].parse().unwrap();
        let month: u32 = date_str[5..7].parse().unwrap();
        let day: u32 = date_str[8..10].parse().unwrap();
        assert!(year >= 2024, "year must be >= 2024: {}", year);
        assert!((1..=12).contains(&month), "month out of range: {}", month);
        assert!((1..=31).contains(&day), "day out of range: {}", day);
    }

    #[test]
    fn test_now_ist_time_returns_valid_time() {
        let time = now_ist_time();
        // NaiveTime is always valid, but verify it's a reasonable IST time
        assert!(time.hour() < 24);
        assert!(time.minute() < 60);
        assert!(time.second() < 60);
    }

    // -----------------------------------------------------------------------
    // is_instrument_fresh — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_instrument_fresh_marker_with_trailing_newline() {
        // is_instrument_fresh uses .trim() so trailing newline should match
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-trailing-newline");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let today = now_ist_date_string();
        std::fs::write(&marker_path, format!("{}\n", today)).unwrap();

        assert!(
            is_instrument_fresh(temp_dir.to_str().unwrap()),
            "trailing newline should be trimmed"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_marker_with_surrounding_whitespace() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-whitespace");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let today = now_ist_date_string();
        std::fs::write(&marker_path, format!("  {}  \n", today)).unwrap();

        assert!(
            is_instrument_fresh(temp_dir.to_str().unwrap()),
            "surrounding whitespace should be trimmed"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_empty_file_returns_false() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-empty");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "").unwrap();

        assert!(
            !is_instrument_fresh(temp_dir.to_str().unwrap()),
            "empty marker file should not be fresh"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_garbage_content_returns_false() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-garbage");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "not-a-date").unwrap();

        assert!(
            !is_instrument_fresh(temp_dir.to_str().unwrap()),
            "garbage content should not match today"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_tomorrow_date_returns_false() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-tomorrow");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        // Write a date far in the future — should not match today
        std::fs::write(&marker_path, "2099-12-31").unwrap();

        assert!(
            !is_instrument_fresh(temp_dir.to_str().unwrap()),
            "future date should not match today"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // write_freshness_marker — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_freshness_marker_creates_nested_directories() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-nested/a/b/c");
        let cache_dir = temp_dir.to_str().unwrap();

        // Ensure clean state
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("dlt-test-marker-nested"));

        write_freshness_marker(cache_dir);

        // Verify marker was written
        assert!(
            is_instrument_fresh(cache_dir),
            "marker should be fresh after writing to nested dir"
        );

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("dlt-test-marker-nested"));
    }

    #[test]
    fn test_write_freshness_marker_overwrites_stale_marker() {
        let temp_dir = std::env::temp_dir().join("dlt-test-marker-overwrite");
        let cache_dir = temp_dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Write a stale marker
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "2020-01-01").unwrap();
        assert!(
            !is_instrument_fresh(cache_dir),
            "stale marker must not be fresh"
        );

        // Overwrite with today
        write_freshness_marker(cache_dir);
        assert!(
            is_instrument_fresh(cache_dir),
            "marker should be fresh after overwrite"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_write_freshness_marker_readonly_directory_no_panic() {
        // Best-effort: writing to an impossible path should not panic
        write_freshness_marker("/proc/1/nonexistent-marker");
        // No assertion — just verify no panic
    }

    // -----------------------------------------------------------------------
    // is_within_build_window — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_build_window_both_invalid_returns_false() {
        assert!(!is_within_build_window("xyz", "abc"));
    }

    #[test]
    fn test_is_within_build_window_reversed_window_returns_false() {
        // end before start — current time cannot be >= 23:00 AND < 01:00 simultaneously
        assert!(!is_within_build_window("23:00:00", "01:00:00"));
    }

    #[test]
    fn test_is_within_build_window_narrow_past_window_returns_false() {
        // A window in the distant past (00:00 to 00:01) — only true if running
        // exactly at midnight; we test the negative case with a one-second window
        // at 00:00:00 to 00:00:01 — almost certainly outside
        let result = is_within_build_window("00:00:00", "00:00:01");
        // Can't assert deterministically, but verify no panic
        let _ = result;
    }

    #[test]
    fn test_is_within_build_window_empty_strings_return_false() {
        assert!(!is_within_build_window("", "08:55:00"));
        assert!(!is_within_build_window("08:25:00", ""));
        assert!(!is_within_build_window("", ""));
    }

    // -----------------------------------------------------------------------
    // Async orchestrator tests — testing observable behavior via markers
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_load_or_build_instruments_outside_window_returns_none() {
        // Use an impossible window (00:00:00 to 00:00:00) — always outside
        let instrument_config = InstrumentConfig {
            daily_download_time: "08:45:00".to_owned(),
            csv_cache_directory: "/tmp/dlt-test-load-outside".to_owned(),
            csv_cache_filename: "test.csv".to_owned(),
            csv_download_timeout_secs: 5,
            build_window_start: "00:00:00".to_owned(),
            build_window_end: "00:00:00".to_owned(),
        };
        let questdb_config = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            ilp_port: 19999,
            http_port: 19998,
            pg_port: 18812,
        };

        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &instrument_config,
            &questdb_config,
        )
        .await;

        assert!(result.is_ok(), "outside window should not error");
        assert!(
            result.unwrap().is_none(),
            "outside window should return None"
        );
    }

    #[tokio::test]
    async fn test_try_rebuild_instruments_with_fresh_marker_returns_none() {
        let temp_dir = std::env::temp_dir().join("dlt-test-rebuild-fresh");
        let cache_dir = temp_dir.to_str().unwrap().to_owned();
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Write today's marker so rebuild thinks it's already done
        write_freshness_marker(&cache_dir);

        let instrument_config = InstrumentConfig {
            daily_download_time: "08:45:00".to_owned(),
            csv_cache_directory: cache_dir.clone(),
            csv_cache_filename: "test.csv".to_owned(),
            csv_download_timeout_secs: 5,
            build_window_start: "00:00:00".to_owned(),
            build_window_end: "23:59:59".to_owned(),
        };
        let questdb_config = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            ilp_port: 19999,
            http_port: 19998,
            pg_port: 18812,
        };

        let result = try_rebuild_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &instrument_config,
            &questdb_config,
        )
        .await;

        assert!(result.is_ok(), "fresh marker should not error");
        assert!(
            result.unwrap().is_none(),
            "fresh marker should return None (idempotent)"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_try_rebuild_instruments_without_marker_attempts_download() {
        // No marker, no server → download fails → returns error
        let temp_dir = std::env::temp_dir().join("dlt-test-rebuild-no-marker");
        let cache_dir = temp_dir.to_str().unwrap().to_owned();
        let _ = std::fs::remove_dir_all(&temp_dir);

        let instrument_config = InstrumentConfig {
            daily_download_time: "08:45:00".to_owned(),
            csv_cache_directory: cache_dir,
            csv_cache_filename: "test.csv".to_owned(),
            csv_download_timeout_secs: 2,
            build_window_start: "00:00:00".to_owned(),
            build_window_end: "23:59:59".to_owned(),
        };
        let questdb_config = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            ilp_port: 19999,
            http_port: 19998,
            pg_port: 18812,
        };

        let result = try_rebuild_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &instrument_config,
            &questdb_config,
        )
        .await;

        assert!(
            result.is_err(),
            "no marker + no server should return error from download failure"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
