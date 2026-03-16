//! JSON cache for index constituency data.
//!
//! Saves and loads the `IndexConstituencyMap` as JSON for fallback
//! when niftyindices.com is unreachable. Atomic write prevents
//! corrupt cache on crash.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;
use tracing::info;

use dhan_live_trader_common::instrument_types::IndexConstituencyMap;

/// Save constituency map to a JSON cache file.
///
/// Uses atomic write: writes to `.tmp` then renames to final path.
pub async fn save_constituency_cache(
    map: &IndexConstituencyMap,
    cache_dir: &str,
    filename: &str,
) -> Result<()> {
    let dir = Path::new(cache_dir);
    fs::create_dir_all(dir)
        .await
        .context("failed to create constituency cache directory")?;

    let final_path = dir.join(filename);
    let tmp_path = dir.join(format!("{filename}.tmp"));

    let json = serde_json::to_string(map).context("failed to serialize constituency map")?;

    fs::write(&tmp_path, json.as_bytes())
        .await
        .context("failed to write constituency cache tmp file")?;

    fs::rename(&tmp_path, &final_path)
        .await
        .context("failed to rename constituency cache tmp to final")?;

    info!(
        path = %final_path.display(),
        indices = map.index_count(),
        stocks = map.stock_count(),
        "constituency cache saved"
    );

    Ok(())
}

/// Load constituency map from a JSON cache file.
pub async fn load_constituency_cache(
    cache_dir: &str,
    filename: &str,
) -> Result<IndexConstituencyMap> {
    let path = cache_path(cache_dir, filename);

    let json = fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read constituency cache at {}", path.display()))?;

    let map: IndexConstituencyMap =
        serde_json::from_str(&json).context("failed to parse constituency cache JSON")?;

    info!(
        path = %path.display(),
        indices = map.index_count(),
        stocks = map.stock_count(),
        "constituency cache loaded"
    );

    Ok(map)
}

/// Build the full cache file path.
fn cache_path(cache_dir: &str, filename: &str) -> PathBuf {
    Path::new(cache_dir).join(filename)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique temp directory for each test.
    fn test_cache_dir(test_name: &str) -> String {
        let dir = format!(
            "/tmp/dlt-test-constituency-{}-{}",
            test_name,
            std::process::id()
        );
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_cache_save_and_load_roundtrip() {
        let cache_dir = test_cache_dir("roundtrip");

        let mut map = IndexConstituencyMap::default();
        map.index_to_constituents.insert(
            "Nifty 50".to_string(),
            vec![
                dhan_live_trader_common::instrument_types::IndexConstituent {
                    index_name: "Nifty 50".to_string(),
                    symbol: "RELIANCE".to_string(),
                    isin: "INE002A01018".to_string(),
                    weight: 10.0,
                    sector: "Energy".to_string(),
                    last_updated: chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(),
                },
            ],
        );
        map.stock_to_indices
            .insert("RELIANCE".to_string(), vec!["Nifty 50".to_string()]);

        save_constituency_cache(&map, &cache_dir, "test-cache.json")
            .await
            .unwrap();

        let loaded = load_constituency_cache(&cache_dir, "test-cache.json")
            .await
            .unwrap();

        assert_eq!(loaded.index_count(), 1);
        assert_eq!(loaded.stock_count(), 1);
        assert!(loaded.contains_index("Nifty 50"));
        assert!(loaded.contains_stock("RELIANCE"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_cache_load_missing_file_returns_error() {
        let result = load_constituency_cache("/nonexistent/path", "missing.json").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cache_load_corrupt_json_returns_error() {
        let cache_dir = test_cache_dir("corrupt");
        let path = Path::new(&cache_dir).join("corrupt.json");
        tokio::fs::write(&path, b"not valid json{{{").await.unwrap();

        let result = load_constituency_cache(&cache_dir, "corrupt.json").await;
        assert!(result.is_err());

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
