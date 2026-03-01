//! Three-layer instrument defense: Time Gate + Freshness Marker + Manual API.
//!
//! Ensures instruments build ONCE at 8:30 AM boot. During trading-hours
//! restarts, zero overhead — not even a file read.
//!
//! # Three Layers
//! 1. **Time Gate** — sub-nanosecond integer comparison. Outside [08:25, 08:55]
//!    IST? Skip entirely.
//! 2. **Freshness Marker** — one file read (~100μs). Already built today? Load
//!    from cache, skip download + persist.
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
// Layer 1: Time Gate
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
// Layer 2: Freshness Marker
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
/// Returns `Ok(None)` if time-gated skip (outside build window).
///
/// `was_fresh_build` is `true` when a full download + persist happened (first
/// build of the day). `false` when loaded from cache (marker was fresh).
pub async fn load_or_build_instruments(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
) -> Result<Option<(FnoUniverse, bool)>> {
    // Layer 1: Time gate
    if !is_within_build_window(
        &instrument_config.build_window_start,
        &instrument_config.build_window_end,
    ) {
        info!(
            window_start = %instrument_config.build_window_start,
            window_end = %instrument_config.build_window_end,
            "instruments: TIME-GATED SKIP (outside build window)"
        );
        return Ok(None);
    }

    // Layer 2: Freshness marker
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

    // Full build: download → parse → build → persist → marker
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
#[allow(clippy::expect_used)] // Compile-time constant — IST_UTC_OFFSET_SECONDS is always valid
fn now_ist_time() -> NaiveTime {
    let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset constant is valid");
    Utc::now().with_timezone(&ist).time()
}

/// Returns today's IST date as `YYYY-MM-DD`.
#[allow(clippy::expect_used)] // Compile-time constant — IST_UTC_OFFSET_SECONDS is always valid
fn now_ist_date_string() -> String {
    let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset constant is valid");
    Utc::now().with_timezone(&ist).date_naive().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
