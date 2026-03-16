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

use chrono::{FixedOffset, Utc};
use tracing::{info, warn};

use dhan_live_trader_common::config::IndexConstituencyConfig;
use dhan_live_trader_common::constants::{
    INDEX_CONSTITUENCY_CSV_CACHE_FILENAME, INDEX_CONSTITUENCY_SLUGS, IST_UTC_OFFSET_SECONDS,
};
use dhan_live_trader_common::instrument_types::{ConstituencyBuildMetadata, IndexConstituencyMap};

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

    #[allow(clippy::expect_used)] // APPROVED: compile-time provable — 19800 always valid
    let ist =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid"); // APPROVED: compile-time provable constant

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
