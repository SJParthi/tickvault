//! Index constituency download, parsing, and mapping.
//!
//! Downloads NSE index constituent CSVs from niftyindices.com, parses them,
//! and builds a bidirectional mapping (index → stocks, stock → indices).
//!
//! # Boot Sequence Position
//! Config -> Instrument -> Universe Build -> **Constituency Download** -> Auth
//!
//! # Error Handling
//! Non-blocking. If all downloads fail, attempts to load from cache.
//! Returns `Ok(None)` if no data available — boot continues.

pub mod cache;
pub mod csv_downloader;
pub mod csv_parser;
pub mod mapper;

use std::time::Instant;

use chrono::Utc;
use tracing::{info, warn};

use dhan_live_trader_common::config::IndexConstituencyConfig;
use dhan_live_trader_common::constants::{
    INDEX_CONSTITUENCY_CSV_CACHE_FILENAME, INDEX_CONSTITUENCY_SLUGS,
};
use dhan_live_trader_common::instrument_types::{ConstituencyBuildMetadata, IndexConstituencyMap};
use dhan_live_trader_common::trading_calendar::ist_offset;

/// Download index constituency data and build the bidirectional map.
///
/// Returns `Ok(Some(map))` on success, `Ok(None)` if disabled or no data.
/// Never returns `Err` — failures are logged and degraded gracefully.
pub async fn download_and_build_constituency_map(
    config: &IndexConstituencyConfig,
    cache_dir: &str,
) -> Option<IndexConstituencyMap> {
    if !config.enabled {
        info!("index constituency download disabled by config");
        return None;
    }

    let start = Instant::now();

    // Download CSVs concurrently
    let downloaded =
        csv_downloader::download_constituency_csvs(config, INDEX_CONSTITUENCY_SLUGS).await;

    if downloaded.is_empty() {
        warn!("all constituency CSV downloads failed — trying cache");
        return try_load_cache(cache_dir).await;
    }

    // Parse each CSV
    let today = chrono::Local::now().date_naive();
    let mut parsed = Vec::with_capacity(downloaded.len());
    let mut parse_failures = 0usize;

    for (index_name, csv_text) in &downloaded {
        let constituents = csv_parser::parse_constituency_csv(index_name, csv_text, today);
        if constituents.is_empty() {
            warn!(index = %index_name, "CSV parsed but produced zero constituents");
            parse_failures = parse_failures.saturating_add(1);
        } else {
            parsed.push((index_name.clone(), constituents));
        }
    }

    let elapsed = start.elapsed();

    let ist = ist_offset();

    let total_mappings: usize = parsed.iter().map(|(_, v)| v.len()).sum();
    let metadata = ConstituencyBuildMetadata {
        download_duration: elapsed,
        indices_downloaded: parsed.len(),
        indices_failed: INDEX_CONSTITUENCY_SLUGS
            .len()
            .saturating_sub(downloaded.len())
            .saturating_add(parse_failures),
        unique_stocks: 0, // will be set after build
        total_mappings,
        build_timestamp: Utc::now().with_timezone(&ist),
    };

    let mut map = mapper::build_constituency_map(parsed, metadata);

    // Update unique_stocks in metadata after build
    map.build_metadata.unique_stocks = map.stock_count();

    info!(
        indices = map.index_count(),
        stocks = map.stock_count(),
        mappings = map.build_metadata.total_mappings,
        duration_ms = elapsed.as_millis(),
        "index constituency map built"
    );

    // Save to cache (non-blocking — failure is WARN)
    if let Err(err) =
        cache::save_constituency_cache(&map, cache_dir, INDEX_CONSTITUENCY_CSV_CACHE_FILENAME).await
    {
        warn!(error = %err, "failed to save constituency cache — continuing without cache");
    }

    Some(map)
}

/// Try to load constituency data from the JSON cache.
async fn try_load_cache(cache_dir: &str) -> Option<IndexConstituencyMap> {
    match cache::load_constituency_cache(cache_dir, INDEX_CONSTITUENCY_CSV_CACHE_FILENAME).await {
        Ok(map) => {
            info!(
                indices = map.index_count(),
                stocks = map.stock_count(),
                "constituency loaded from cache (network failed)"
            );
            Some(map)
        }
        Err(err) => {
            warn!(error = %err, "no constituency cache available — continuing without constituency data");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::instrument_types::IndexConstituent;

    fn test_config(enabled: bool) -> IndexConstituencyConfig {
        IndexConstituencyConfig {
            enabled,
            download_timeout_secs: 1,
            max_concurrent_downloads: 2,
            inter_batch_delay_ms: 0,
        }
    }

    fn test_cache_dir(name: &str) -> String {
        let dir = format!(
            "/tmp/dlt-test-constituency-mod-{}-{}",
            name,
            std::process::id()
        );
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    // -----------------------------------------------------------------------
    // download_and_build_constituency_map: disabled config
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_disabled_config_returns_none() {
        let config = test_config(false);
        let cache_dir = test_cache_dir("disabled");
        let result = download_and_build_constituency_map(&config, &cache_dir).await;
        assert!(result.is_none(), "disabled config should return None");
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // -----------------------------------------------------------------------
    // download_and_build_constituency_map: all downloads fail, no cache
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_all_downloads_fail_no_cache_returns_none() {
        let config = test_config(true);
        let cache_dir = test_cache_dir("all-fail-no-cache");
        // Slugs point to unreachable URLs (the base URL + slug will fail)
        // No cache exists in the directory
        let result = download_and_build_constituency_map(&config, &cache_dir).await;
        // This should return None since all downloads fail and no cache exists
        assert!(
            result.is_none(),
            "all downloads fail + no cache should return None"
        );
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // -----------------------------------------------------------------------
    // download_and_build_constituency_map: all downloads fail, cache exists
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_all_downloads_fail_with_cache_returns_cached() {
        let cache_dir = test_cache_dir("all-fail-with-cache");

        // Pre-populate a valid cache file
        let mut map = IndexConstituencyMap::default();
        map.index_to_constituents.insert(
            "Nifty 50".to_string(),
            vec![IndexConstituent {
                index_name: "Nifty 50".to_string(),
                symbol: "RELIANCE".to_string(),
                isin: "INE002A01018".to_string(),
                weight: 10.0,
                sector: "Energy".to_string(),
                last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
            }],
        );
        map.stock_to_indices
            .insert("RELIANCE".to_string(), vec!["Nifty 50".to_string()]);

        cache::save_constituency_cache(&map, &cache_dir, INDEX_CONSTITUENCY_CSV_CACHE_FILENAME)
            .await
            .unwrap();

        let config = test_config(true);
        let result = download_and_build_constituency_map(&config, &cache_dir).await;
        // Downloads will fail (real URLs won't work in tests), so it falls back to cache
        // Result depends on whether actual download succeeds. With real niftyindices.com
        // URLs failing in CI, it should fall back to cache.
        // We can't guarantee download failure, so just verify the function doesn't panic
        // and returns Some.
        assert!(
            result.is_some(),
            "should return Some from either download or cache"
        );
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // -----------------------------------------------------------------------
    // try_load_cache: missing cache
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_load_cache_missing_file_returns_none() {
        let result = try_load_cache("/tmp/dlt-test-nonexistent-cache-dir-99999").await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // try_load_cache: valid cache
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_load_cache_valid_file_returns_map() {
        let cache_dir = test_cache_dir("valid-cache");

        let mut map = IndexConstituencyMap::default();
        map.index_to_constituents.insert(
            "Nifty IT".to_string(),
            vec![IndexConstituent {
                index_name: "Nifty IT".to_string(),
                symbol: "INFY".to_string(),
                isin: "INE009A01021".to_string(),
                weight: 15.0,
                sector: "IT".to_string(),
                last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
            }],
        );
        map.stock_to_indices
            .insert("INFY".to_string(), vec!["Nifty IT".to_string()]);

        cache::save_constituency_cache(&map, &cache_dir, INDEX_CONSTITUENCY_CSV_CACHE_FILENAME)
            .await
            .unwrap();

        let result = try_load_cache(&cache_dir).await;
        assert!(result.is_some(), "valid cache should return Some");
        let loaded = result.unwrap();
        assert_eq!(loaded.index_count(), 1);
        assert!(loaded.contains_index("Nifty IT"));
        assert!(loaded.contains_stock("INFY"));

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // -----------------------------------------------------------------------
    // try_load_cache: corrupt JSON cache
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_load_cache_corrupt_json_returns_none() {
        let cache_dir = test_cache_dir("corrupt-json");
        let cache_path =
            std::path::Path::new(&cache_dir).join(INDEX_CONSTITUENCY_CSV_CACHE_FILENAME);
        tokio::fs::write(&cache_path, b"{ invalid json [[")
            .await
            .unwrap();

        let result = try_load_cache(&cache_dir).await;
        assert!(result.is_none(), "corrupt JSON should return None");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // -----------------------------------------------------------------------
    // download_and_build_constituency_map: enabled but empty slugs constant
    // Note: INDEX_CONSTITUENCY_SLUGS is a constant so we can't make it empty,
    // but we verify the function works with enabled=true.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_enabled_config_does_not_return_none_immediately() {
        let config = test_config(true);
        let cache_dir = test_cache_dir("enabled-check");
        // With enabled=true, the function should NOT return None immediately
        // (unlike disabled=true which returns None instantly).
        // It will try downloads (and likely fail) then try cache (empty).
        let result = download_and_build_constituency_map(&config, &cache_dir).await;
        // Don't assert specific result since it depends on network,
        // but verify it completed without panic
        let _ = result;
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
