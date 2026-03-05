//! rkyv binary cache for `FnoUniverse`.
//!
//! Serialize/deserialize the instrument universe to a memory-mapped binary file
//! for fast restart during market hours. Uses rkyv `from_bytes` for validated
//! deserialization (~5-15ms, 25x faster than CSV re-parse).

use std::path::Path;

use anyhow::{Context, Result};
use memmap2::Mmap;
use tracing::info;

use dhan_live_trader_common::constants::BINARY_CACHE_FILENAME;
use dhan_live_trader_common::instrument_types::FnoUniverse;

/// Serialize `FnoUniverse` to rkyv binary file.
///
/// Called after successful CSV build. Best-effort — caller should log and
/// continue if this fails.
pub fn write_binary_cache(universe: &FnoUniverse, cache_dir: &str) -> Result<()> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(universe)
        .map_err(|err| anyhow::anyhow!("rkyv serialize failed: {err}"))?;
    let path = Path::new(cache_dir).join(BINARY_CACHE_FILENAME);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir: {}", parent.display()))?;
    }
    std::fs::write(&path, &bytes)
        .with_context(|| format!("failed to write rkyv cache: {}", path.display()))?;
    info!(
        bytes = bytes.len(),
        path = %path.display(),
        "rkyv binary cache written"
    );
    Ok(())
}

/// Load `FnoUniverse` from rkyv binary cache via mmap.
///
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file exists but is corrupt or unreadable.
pub fn read_binary_cache(cache_dir: &str) -> Result<Option<FnoUniverse>> {
    let path = Path::new(cache_dir).join(BINARY_CACHE_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(&path)
        .with_context(|| format!("failed to open rkyv cache: {}", path.display()))?;

    let mmap = unsafe { Mmap::map(&file) } // SAFETY: read-only file, single-process, no concurrent writers
        .with_context(|| format!("failed to mmap rkyv cache: {}", path.display()))?;

    let universe = rkyv::from_bytes::<FnoUniverse, rkyv::rancor::Error>(&mmap)
        .map_err(|err| anyhow::anyhow!("rkyv deserialize failed (corrupt cache?): {err}"))?;

    info!(
        bytes = mmap.len(),
        derivatives = universe.derivative_contracts.len(),
        underlyings = universe.underlyings.len(),
        "rkyv binary cache loaded"
    );
    Ok(Some(universe))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::instrument_types::UniverseBuildMetadata;
    use std::collections::HashMap;

    /// Build a minimal FnoUniverse for testing.
    fn test_universe() -> FnoUniverse {
        use chrono::{FixedOffset, Utc};
        let ist = FixedOffset::east_opt(19_800).unwrap(); // APPROVED: test constant
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

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dlt-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn test_write_and_read_binary_cache_roundtrip() {
        let dir = unique_temp_dir("rkyv-roundtrip");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let original = test_universe();
        write_binary_cache(&original, cache_dir).unwrap();

        let loaded = read_binary_cache(cache_dir).unwrap().unwrap();

        assert_eq!(loaded.build_metadata.csv_source, "test");
        assert_eq!(loaded.build_metadata.csv_row_count, 100);
        assert_eq!(loaded.build_metadata.parsed_row_count, 50);
        assert_eq!(
            loaded.build_metadata.build_duration,
            std::time::Duration::from_millis(42)
        );
        assert_eq!(loaded.underlyings.len(), 0);
        assert_eq!(loaded.derivative_contracts.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_binary_cache_missing_returns_none() {
        let result = read_binary_cache("/tmp/dlt-nonexistent-rkyv-cache-98765").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_binary_cache_corrupt_returns_error() {
        let dir = unique_temp_dir("rkyv-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        std::fs::write(&path, b"this is garbage data not rkyv").unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(result.is_err(), "corrupt rkyv file should return Err");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_binary_cache_empty_returns_error() {
        let dir = unique_temp_dir("rkyv-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        std::fs::write(&path, b"").unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(result.is_err(), "empty rkyv file should return Err");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_binary_cache_creates_directories() {
        let base = unique_temp_dir("rkyv-nested");
        let _ = std::fs::remove_dir_all(&base);
        let nested = base.join("a/b/c");
        let cache_dir = nested.to_str().unwrap();

        let universe = test_universe();
        write_binary_cache(&universe, cache_dir).unwrap();

        let path = nested.join(BINARY_CACHE_FILENAME);
        assert!(
            path.exists(),
            "binary cache file should exist in nested dir"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
