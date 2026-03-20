//! Market-hours-aware instrument loader with rkyv binary cache.
//!
//! # Semantics
//! - **Market hours** `[09:00, 15:30)` IST — cache first, emergency download
//!   as last resort (I-P0-06). Load order:
//!   1. rkyv zero-copy binary cache (sub-0.5ms via `MappedUniverse`)
//!   2. CSV file cache fallback (~400ms, then writes rkyv for next restart)
//!   3. Emergency download (all caches missing — logs CRITICAL)
//!   4. `Unavailable` — all sources exhausted
//! - **Outside market hours** — ALWAYS download fresh CSV (ignores freshness
//!   marker). On download failure, falls back to rkyv → CSV → error.
//! - **Manual API** — `POST /api/instruments/rebuild`. Bypasses market hours
//!   check, always downloads, respects freshness marker (idempotent).
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{NaiveTime, Utc};
use tracing::{error, info, warn};

use dhan_live_trader_common::config::{InstrumentConfig, QuestDbConfig, SubscriptionConfig};
use dhan_live_trader_common::constants::INSTRUMENT_FRESHNESS_MARKER_FILENAME;
use dhan_live_trader_common::instrument_types::FnoUniverse;
use dhan_live_trader_common::trading_calendar::ist_offset;

use super::binary_cache::{MappedUniverse, read_binary_cache, write_binary_cache};
use super::csv_downloader::{download_instrument_csv, load_cached_csv};
use super::subscription_planner::{SubscriptionPlan, build_subscription_plan_from_archived};
use super::universe_builder::build_fno_universe_from_csv;

use dhan_live_trader_storage::instrument_persistence::persist_instrument_snapshot;

// ---------------------------------------------------------------------------
// Load Result
// ---------------------------------------------------------------------------

/// Result of the instrument loading process.
///
/// Separates the two performance-critical paths:
/// - **Market hours restart**: zero-copy rkyv → `CachedPlan` (sub-0.5ms)
/// - **Outside market hours**: full download+build → `FreshBuild` (seconds)
pub enum InstrumentLoadResult {
    /// Fresh build with owned universe (outside market hours / manual rebuild).
    /// Caller should build a `SubscriptionPlan` and send notifications.
    FreshBuild(FnoUniverse),
    /// Zero-copy cached plan (market hours restart). Ready to use immediately.
    CachedPlan(SubscriptionPlan),
    /// No instruments available (market hours + no cache).
    Unavailable,
}

// ---------------------------------------------------------------------------
// Market Hours Check (replaces old "build window" concept)
// ---------------------------------------------------------------------------

/// Returns `true` if the current IST time is within the market hours window
/// `[window_start, window_end)` (inclusive start, exclusive end).
///
/// During this window, downloads are BLOCKED — only cached instruments are used.
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
// Freshness Marker
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

/// Main entry: market-hours-aware instrument loading.
///
/// Returns:
/// - `InstrumentLoadResult::FreshBuild(universe)` — outside market hours, full download+build
/// - `InstrumentLoadResult::CachedPlan(plan)` — market hours, zero-copy rkyv or CSV fallback
/// - `InstrumentLoadResult::Unavailable` — market hours, no cache available
pub async fn load_or_build_instruments(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
    subscription_config: &SubscriptionConfig,
) -> Result<InstrumentLoadResult> {
    if is_within_build_window(
        &instrument_config.build_window_start,
        &instrument_config.build_window_end,
    ) {
        // ----- MARKET HOURS: cache first, emergency download if both miss -----
        // I-P0-06: Emergency Download Override
        return load_from_cache_or_emergency_download(
            dhan_csv_url,
            dhan_csv_fallback_url,
            instrument_config,
            questdb_config,
            subscription_config,
        )
        .await;
    }

    // ----- OUTSIDE MARKET HOURS: always download fresh -----
    load_with_download(
        dhan_csv_url,
        dhan_csv_fallback_url,
        instrument_config,
        questdb_config,
    )
    .await
}

/// Manual trigger: bypasses market hours check, respects freshness marker.
///
/// Called by `POST /api/instruments/rebuild`.
/// Returns `Ok(None)` if already built today (idempotent).
/// Returns `Ok(Some(universe))` if rebuilt successfully.
pub async fn try_rebuild_instruments(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
) -> Result<Option<FnoUniverse>> {
    let cache_dir = &instrument_config.csv_cache_directory;

    // Respect freshness marker (idempotent)
    if is_instrument_fresh(cache_dir) {
        info!("manual rebuild: already built today, skipping");
        return Ok(None);
    }

    // Full build
    info!("manual rebuild: starting full instrument build");
    let download_result = download_instrument_csv(
        dhan_csv_url,
        dhan_csv_fallback_url,
        cache_dir,
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

    // Write rkyv binary cache for fast restart
    if let Err(err) = write_binary_cache(&universe, cache_dir) {
        warn!(%err, "manual rebuild: rkyv binary cache write failed (non-fatal)");
    }

    write_freshness_marker(cache_dir);

    Ok(Some(universe))
}

// ---------------------------------------------------------------------------
// Internal: Market Hours Cache Loading
// ---------------------------------------------------------------------------

/// Market hours path: zero-copy rkyv → CSV fallback → emergency download → Unavailable.
///
/// I-P0-06: When all caches miss during market hours, force-download as last resort.
/// Never silently run with zero instruments.
async fn load_from_cache_or_emergency_download(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
    subscription_config: &SubscriptionConfig,
) -> Result<InstrumentLoadResult> {
    let cache_dir = &instrument_config.csv_cache_directory;
    let today = Utc::now().with_timezone(&ist_offset()).date_naive();

    // Try 1: Zero-copy rkyv binary cache (sub-0.5ms)
    match MappedUniverse::load(cache_dir) {
        Ok(Some(mapped)) => {
            let plan = build_subscription_plan_from_archived(
                mapped.archived(),
                subscription_config,
                today,
            );
            info!(
                derivatives = mapped.derivative_count(),
                underlyings = mapped.underlying_count(),
                total_subscriptions = plan.summary.total,
                "market hours: zero-copy rkyv loaded (sub-0.5ms)"
            );
            return Ok(InstrumentLoadResult::CachedPlan(plan));
        }
        Ok(None) => {
            info!("market hours: no rkyv binary cache found, trying CSV fallback");
        }
        Err(err) => {
            warn!(%err, "market hours: rkyv binary cache corrupt, trying CSV fallback");
        }
    }

    // Try 2: CSV file cache (~400ms) — build plan from owned universe
    match load_cached_csv(cache_dir, &instrument_config.csv_cache_filename).await {
        Ok(download_result) => {
            let universe =
                build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
                    .context("market hours: failed to build universe from cached CSV")?;

            // Write rkyv binary cache for next restart
            if let Err(err) = write_binary_cache(&universe, cache_dir) {
                warn!(%err, "market hours: failed to write rkyv cache after CSV load (non-fatal)");
            }

            info!(
                derivatives = universe.derivative_contracts.len(),
                "market hours: loaded instruments from CSV cache (rkyv cache written for next restart)"
            );
            return Ok(InstrumentLoadResult::FreshBuild(universe));
        }
        Err(err) => {
            warn!(%err, "market hours: no CSV cache available either");
        }
    }

    // I-P0-06: Emergency Download Override — all caches missing during market hours
    error!(
        "CRITICAL: no instrument cache available during market hours — triggering emergency download"
    );

    match download_instrument_csv(
        dhan_csv_url,
        dhan_csv_fallback_url,
        cache_dir,
        &instrument_config.csv_cache_filename,
        instrument_config.csv_download_timeout_secs,
    )
    .await
    {
        Ok(download_result) => {
            let universe =
                build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
                    .context("emergency download: universe build failed")?;

            // Persist to QuestDB (best-effort)
            if let Err(err) = persist_instrument_snapshot(&universe, questdb_config).await {
                warn!(%err, "emergency download: persistence failed (non-fatal)");
            }

            // Write rkyv binary cache for subsequent restarts
            if let Err(err) = write_binary_cache(&universe, cache_dir) {
                warn!(%err, "emergency download: rkyv cache write failed (non-fatal)");
            }

            write_freshness_marker(cache_dir);

            error!(
                derivatives = universe.derivative_contracts.len(),
                "emergency download succeeded — instruments loaded (investigate why cache was missing)"
            );
            return Ok(InstrumentLoadResult::FreshBuild(universe));
        }
        Err(err) => {
            error!(
                %err,
                "CRITICAL: emergency download ALSO failed — system will have ZERO instruments"
            );
        }
    }

    // All sources exhausted — truly unavailable
    Ok(InstrumentLoadResult::Unavailable)
}

// ---------------------------------------------------------------------------
// Internal: Outside Market Hours Download
// ---------------------------------------------------------------------------

/// Outside market hours: always download fresh, fallback to caches on failure.
async fn load_with_download(
    dhan_csv_url: &str,
    dhan_csv_fallback_url: &str,
    instrument_config: &InstrumentConfig,
    questdb_config: &QuestDbConfig,
) -> Result<InstrumentLoadResult> {
    let cache_dir = &instrument_config.csv_cache_directory;

    // Always attempt download (ignore freshness marker outside market hours)
    info!("outside market hours: downloading fresh instrument CSV");
    match download_instrument_csv(
        dhan_csv_url,
        dhan_csv_fallback_url,
        cache_dir,
        &instrument_config.csv_cache_filename,
        instrument_config.csv_download_timeout_secs,
    )
    .await
    {
        Ok(download_result) => {
            let universe =
                build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
                    .context("outside market hours: universe build failed")?;

            // Persist to QuestDB (best-effort)
            if let Err(err) = persist_instrument_snapshot(&universe, questdb_config).await {
                warn!(%err, "instrument snapshot persistence failed (non-fatal)");
            }

            // Write rkyv binary cache for fast market-hours restart
            if let Err(err) = write_binary_cache(&universe, cache_dir) {
                warn!(%err, "rkyv binary cache write failed (non-fatal)");
            }

            write_freshness_marker(cache_dir);

            return Ok(InstrumentLoadResult::FreshBuild(universe));
        }
        Err(err) => {
            warn!(%err, "outside market hours: CSV download failed, trying caches");
        }
    }

    // Download failed — fall back to caches
    // Try rkyv binary cache
    match read_binary_cache(cache_dir) {
        Ok(Some(universe)) => {
            warn!(
                derivatives = universe.derivative_contracts.len(),
                "outside market hours: download failed, using rkyv binary cache fallback"
            );
            return Ok(InstrumentLoadResult::FreshBuild(universe));
        }
        Ok(None) => {}
        Err(err) => {
            warn!(%err, "outside market hours: rkyv cache also corrupt");
        }
    }

    // Try CSV file cache
    if let Ok(download_result) =
        load_cached_csv(cache_dir, &instrument_config.csv_cache_filename).await
    {
        let universe =
            build_fno_universe_from_csv(&download_result.csv_text, &download_result.source)
                .context("outside market hours: failed to build universe from stale CSV")?;
        warn!(
            derivatives = universe.derivative_contracts.len(),
            "outside market hours: download failed, using stale CSV cache fallback"
        );
        return Ok(InstrumentLoadResult::FreshBuild(universe));
    }

    // All sources failed
    anyhow::bail!("outside market hours: all instrument sources failed (download + rkyv + CSV)")
}

// ---------------------------------------------------------------------------
// Internal Helpers
// ---------------------------------------------------------------------------

/// Returns the current IST time.
fn now_ist_time() -> NaiveTime {
    Utc::now().with_timezone(&ist_offset()).time()
}

/// Returns today's IST date as `YYYY-MM-DD`.
fn now_ist_date_string() -> String {
    Utc::now()
        .with_timezone(&ist_offset())
        .date_naive()
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use dhan_live_trader_common::config::SubscriptionConfig;
    use dhan_live_trader_common::constants::BINARY_CACHE_FILENAME;
    use dhan_live_trader_common::instrument_types::UniverseBuildMetadata;
    use std::collections::HashMap;

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dlt-test-loader-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn test_universe() -> FnoUniverse {
        use chrono::Utc;
        let ist = ist_offset();
        FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 100,
                parsed_row_count: 50,
                index_count: 8,
                equity_count: 20,
                underlying_count: 5,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::from_millis(42),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        }
    }

    fn test_instrument_config(
        cache_dir: &str,
        window_start: &str,
        window_end: &str,
    ) -> InstrumentConfig {
        InstrumentConfig {
            daily_download_time: "08:45:00".to_owned(),
            csv_cache_directory: cache_dir.to_owned(),
            csv_cache_filename: "test.csv".to_owned(),
            csv_download_timeout_secs: 2,
            build_window_start: window_start.to_owned(),
            build_window_end: window_end.to_owned(),
        }
    }

    fn test_questdb_config() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            ilp_port: 19999,
            http_port: 19998,
            pg_port: 18812,
        }
    }

    fn test_subscription_config() -> SubscriptionConfig {
        SubscriptionConfig::default()
    }

    // -----------------------------------------------------------------------
    // is_within_build_window
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_build_window_full_day_window() {
        assert!(is_within_build_window("00:00:00", "23:59:59"));
    }

    #[test]
    fn test_is_within_build_window_impossible_window() {
        assert!(!is_within_build_window("12:00:00", "12:00:00"));
    }

    #[test]
    fn test_is_within_build_window_invalid_format() {
        assert!(!is_within_build_window("bad", "08:55:00"));
        assert!(!is_within_build_window("08:25:00", "bad"));
    }

    // -----------------------------------------------------------------------
    // Freshness marker
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_instrument_fresh_no_marker() {
        assert!(!is_instrument_fresh("/tmp/dlt-nonexistent-marker-98765"));
    }

    #[test]
    fn test_is_instrument_fresh_stale_marker() {
        let temp_dir = unique_temp_dir("stale-marker");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "2020-01-01").unwrap();
        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_today_marker() {
        let temp_dir = unique_temp_dir("today-marker");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let today = now_ist_date_string();
        std::fs::write(&marker_path, &today).unwrap();
        assert!(is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_write_freshness_marker_roundtrip() {
        let temp_dir = unique_temp_dir("marker-roundtrip");
        let cache_dir = temp_dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);
        assert!(!is_instrument_fresh(cache_dir));
        write_freshness_marker(cache_dir);
        assert!(is_instrument_fresh(cache_dir));
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(content, now_ist_date_string());
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // now_ist_date_string / now_ist_time
    // -----------------------------------------------------------------------

    #[test]
    fn test_now_ist_date_string_format_yyyy_mm_dd() {
        let date_str = now_ist_date_string();
        assert_eq!(date_str.len(), 10);
        assert_eq!(date_str.chars().nth(4), Some('-'));
        assert_eq!(date_str.chars().nth(7), Some('-'));
    }

    #[test]
    fn test_now_ist_time_returns_valid_time() {
        let time = now_ist_time();
        assert!(time.hour() < 24);
        assert!(time.minute() < 60);
    }

    // -----------------------------------------------------------------------
    // Freshness marker — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_instrument_fresh_marker_with_trailing_newline() {
        let temp_dir = unique_temp_dir("marker-newline");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let today = now_ist_date_string();
        std::fs::write(&marker_path, format!("{}\n", today)).unwrap();
        assert!(is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_empty_file_returns_false() {
        let temp_dir = unique_temp_dir("marker-empty");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "").unwrap();
        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_write_freshness_marker_creates_nested_directories() {
        let base_dir = unique_temp_dir("marker-nested");
        let temp_dir = base_dir.join("a/b/c");
        let cache_dir = temp_dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&base_dir);
        write_freshness_marker(cache_dir);
        assert!(is_instrument_fresh(cache_dir));
        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_write_freshness_marker_readonly_directory_no_panic() {
        write_freshness_marker("/proc/1/nonexistent-marker");
    }

    // -----------------------------------------------------------------------
    // is_within_build_window — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_build_window_both_invalid_returns_false() {
        assert!(!is_within_build_window("xyz", "abc"));
    }

    #[test]
    fn test_is_within_build_window_reversed_window_returns_false() {
        assert!(!is_within_build_window("23:00:00", "01:00:00"));
    }

    #[test]
    fn test_is_within_build_window_empty_strings_return_false() {
        assert!(!is_within_build_window("", "08:55:00"));
        assert!(!is_within_build_window("08:25:00", ""));
        assert!(!is_within_build_window("", ""));
    }

    // -----------------------------------------------------------------------
    // Market hours: rkyv cache loads
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_market_hours_rkyv_cache_loads() {
        let dir = unique_temp_dir("mh-rkyv-loads");
        let _ = std::fs::remove_dir_all(&dir);
        let cache_dir = dir.to_str().unwrap();

        // Pre-write rkyv cache
        let universe = test_universe();
        write_binary_cache(&universe, cache_dir).unwrap();

        // Window = always inside (00:00 to 23:59)
        let config = test_instrument_config(cache_dir, "00:00:00", "23:59:59");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        match result.unwrap() {
            InstrumentLoadResult::CachedPlan(_plan) => {
                // Zero-copy path: returns plan directly
            }
            other => panic!(
                "expected CachedPlan, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Market hours: no rkyv, no CSV → None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_market_hours_no_cache_returns_unavailable() {
        let dir = unique_temp_dir("mh-no-cache");
        let _ = std::fs::remove_dir_all(&dir);
        let cache_dir = dir.to_str().unwrap();

        let config = test_instrument_config(cache_dir, "00:00:00", "23:59:59");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        assert!(
            matches!(result.unwrap(), InstrumentLoadResult::Unavailable),
            "no cache should return Unavailable during market hours"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Market hours: corrupt rkyv → CSV fallback writes new rkyv
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_market_hours_corrupt_rkyv_no_csv_returns_unavailable() {
        let dir = unique_temp_dir("mh-corrupt-rkyv");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache_dir = dir.to_str().unwrap();

        // Write corrupt rkyv
        let rkyv_path = dir.join(BINARY_CACHE_FILENAME);
        std::fs::write(&rkyv_path, b"garbage").unwrap();

        let config = test_instrument_config(cache_dir, "00:00:00", "23:59:59");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        assert!(
            matches!(result.unwrap(), InstrumentLoadResult::Unavailable),
            "corrupt rkyv + no CSV → Unavailable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Market hours: emergency download when cache missing (I-P0-06)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_market_hours_emergency_download_on_cache_miss() {
        let dir = unique_temp_dir("mh-emergency-download");
        let _ = std::fs::remove_dir_all(&dir);
        let cache_dir = dir.to_str().unwrap();

        // Window = always inside, fake URLs, no cache
        // I-P0-06: emergency download is attempted (and fails with fake URLs),
        // so result is still Unavailable but download WAS attempted.
        let config = test_instrument_config(cache_dir, "00:00:00", "23:59:59");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        assert!(
            matches!(result.unwrap(), InstrumentLoadResult::Unavailable),
            "emergency download with fake URLs should still return Unavailable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Outside hours: download fails → rkyv fallback
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_outside_hours_download_fails_rkyv_fallback() {
        let dir = unique_temp_dir("oh-rkyv-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        let cache_dir = dir.to_str().unwrap();

        // Pre-write rkyv cache
        let universe = test_universe();
        write_binary_cache(&universe, cache_dir).unwrap();

        // Window = always outside (00:00 to 00:00)
        let config = test_instrument_config(cache_dir, "00:00:00", "00:00:00");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        match result.unwrap() {
            InstrumentLoadResult::FreshBuild(loaded) => {
                assert_eq!(loaded.build_metadata.csv_source, "test");
            }
            other => panic!(
                "expected FreshBuild, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Outside hours: all fail → error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_outside_hours_all_fail_returns_error() {
        let dir = unique_temp_dir("oh-all-fail");
        let _ = std::fs::remove_dir_all(&dir);
        let cache_dir = dir.to_str().unwrap();

        // Window = always outside, no cache, fake URLs
        let config = test_instrument_config(cache_dir, "00:00:00", "00:00:00");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        assert!(
            result.is_err(),
            "outside hours with no sources should error"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Manual rebuild: fresh marker returns None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_rebuild_instruments_with_fresh_marker_returns_none() {
        let temp_dir = unique_temp_dir("rebuild-fresh");
        let cache_dir = temp_dir.to_str().unwrap().to_owned();
        let _ = std::fs::remove_dir_all(&temp_dir);
        write_freshness_marker(&cache_dir);

        let config = test_instrument_config(&cache_dir, "00:00:00", "23:59:59");
        let result = try_rebuild_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
        )
        .await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "fresh marker should return None");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // Manual rebuild: no marker → attempts download
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_rebuild_instruments_without_marker_attempts_download() {
        let temp_dir = unique_temp_dir("rebuild-no-marker");
        let cache_dir = temp_dir.to_str().unwrap().to_owned();
        let _ = std::fs::remove_dir_all(&temp_dir);

        let config = test_instrument_config(&cache_dir, "00:00:00", "23:59:59");
        let result = try_rebuild_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
        )
        .await;

        assert!(
            result.is_err(),
            "no marker + no server should error from download failure"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // is_within_build_window — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_build_window_with_seconds_precision() {
        // Very narrow window that likely excludes current time
        assert!(!is_within_build_window("03:00:00", "03:00:01"));
    }

    #[test]
    fn test_is_within_build_window_hour_boundary() {
        // Window from midnight to 1am
        let now = now_ist_time();
        if now.hour() == 0 && now.minute() < 59 {
            assert!(is_within_build_window("00:00:00", "01:00:00"));
        }
        // Always test that invalid formats return false
        assert!(!is_within_build_window("25:00:00", "26:00:00"));
    }

    #[test]
    fn test_is_within_build_window_missing_seconds() {
        // Format without seconds should fail to parse
        assert!(!is_within_build_window("08:25", "08:55"));
    }

    #[test]
    fn test_is_within_build_window_with_extra_chars() {
        assert!(!is_within_build_window("08:25:00am", "08:55:00pm"));
    }

    // -----------------------------------------------------------------------
    // Freshness marker — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_instrument_fresh_garbage_content_returns_false() {
        let temp_dir = unique_temp_dir("marker-garbage");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "not-a-date-string").unwrap();
        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_future_date_returns_false() {
        let temp_dir = unique_temp_dir("marker-future");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "2099-12-31").unwrap();
        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_yesterday_returns_false() {
        let temp_dir = unique_temp_dir("marker-yesterday");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        let yesterday = (Utc::now().with_timezone(&ist_offset()).date_naive()
            - chrono::Duration::days(1))
        .to_string();
        std::fs::write(&marker_path, &yesterday).unwrap();
        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_instrument_fresh_whitespace_only_returns_false() {
        let temp_dir = unique_temp_dir("marker-whitespace");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "   \n  ").unwrap();
        assert!(!is_instrument_fresh(temp_dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_write_freshness_marker_overwrites_old_marker() {
        let temp_dir = unique_temp_dir("marker-overwrite");
        let cache_dir = temp_dir.to_str().unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();

        let marker_path = temp_dir.join(INSTRUMENT_FRESHNESS_MARKER_FILENAME);
        std::fs::write(&marker_path, "2020-01-01").unwrap();
        assert!(!is_instrument_fresh(cache_dir));

        write_freshness_marker(cache_dir);
        assert!(is_instrument_fresh(cache_dir));

        let content = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(content, now_ist_date_string());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // -----------------------------------------------------------------------
    // now_ist_date_string / now_ist_time — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_now_ist_date_string_is_valid_date() {
        let date_str = now_ist_date_string();
        let parsed = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d");
        assert!(parsed.is_ok(), "date string should parse: {}", date_str);
    }

    #[test]
    fn test_now_ist_date_string_not_empty() {
        assert!(!now_ist_date_string().is_empty());
    }

    #[test]
    fn test_now_ist_time_consistency() {
        // Two calls should return times within 1 second of each other
        let t1 = now_ist_time();
        let t2 = now_ist_time();
        let diff = if t2 >= t1 { t2 - t1 } else { t1 - t2 };
        assert!(
            diff < chrono::TimeDelta::seconds(2),
            "consecutive calls should be within 2 seconds"
        );
    }

    // -----------------------------------------------------------------------
    // InstrumentLoadResult discriminant coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_instrument_load_result_variants() {
        let fresh = InstrumentLoadResult::FreshBuild(test_universe());
        assert!(matches!(fresh, InstrumentLoadResult::FreshBuild(_)));

        let unavailable = InstrumentLoadResult::Unavailable;
        assert!(matches!(unavailable, InstrumentLoadResult::Unavailable));
    }

    // -----------------------------------------------------------------------
    // Outside hours: download fails → corrupt rkyv → CSV fallback
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_outside_hours_download_fails_corrupt_rkyv_no_csv_returns_error() {
        let dir = unique_temp_dir("oh-corrupt-rkyv-no-csv");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache_dir = dir.to_str().unwrap();

        // Write corrupt rkyv cache
        let rkyv_path = dir.join(BINARY_CACHE_FILENAME);
        std::fs::write(&rkyv_path, b"not valid rkyv data").unwrap();

        // Window = always outside, fake URLs, corrupt rkyv, no CSV
        let config = test_instrument_config(cache_dir, "00:00:00", "00:00:00");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        // Should error because download fails, rkyv is corrupt, and no CSV
        assert!(
            result.is_err(),
            "corrupt rkyv + no CSV + failed download should error"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Market hours: rkyv corrupt → no CSV → emergency download fails → Unavailable
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_market_hours_corrupt_rkyv_then_emergency_download_fails() {
        let dir = unique_temp_dir("mh-corrupt-emergency-fail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache_dir = dir.to_str().unwrap();

        // Write corrupt rkyv
        let rkyv_path = dir.join(BINARY_CACHE_FILENAME);
        std::fs::write(&rkyv_path, b"corrupt bytes").unwrap();

        let config = test_instrument_config(cache_dir, "00:00:00", "23:59:59");
        let result = load_or_build_instruments(
            "http://127.0.0.1:1/fake",
            "http://127.0.0.1:1/fake",
            &config,
            &test_questdb_config(),
            &test_subscription_config(),
        )
        .await;

        // All caches miss + emergency download fails → Unavailable
        assert!(
            matches!(result.unwrap(), InstrumentLoadResult::Unavailable),
            "all sources exhausted should return Unavailable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Test config helpers produce valid configs
    // -----------------------------------------------------------------------

    #[test]
    fn test_test_instrument_config_has_expected_fields() {
        let config = test_instrument_config("/tmp/test", "08:00:00", "16:00:00");
        assert_eq!(config.csv_cache_directory, "/tmp/test");
        assert_eq!(config.build_window_start, "08:00:00");
        assert_eq!(config.build_window_end, "16:00:00");
        assert_eq!(config.csv_cache_filename, "test.csv");
    }

    #[test]
    fn test_test_universe_has_valid_metadata() {
        let universe = test_universe();
        assert_eq!(universe.build_metadata.csv_source, "test");
        assert_eq!(universe.build_metadata.csv_row_count, 100);
        assert_eq!(universe.build_metadata.parsed_row_count, 50);
        assert!(universe.underlyings.is_empty());
        assert!(universe.derivative_contracts.is_empty());
    }
}
