//! NTM Sub-PR #3 — JSON cache round-trip for the constituency map.
//!
//! **Feature-gated** behind `daily_universe_fetcher`. Serializes an
//! `IndexConstituencyMap` to `<base_dir>/<trading_date>/constituency-map.json`
//! and reads it back. The `trading_date` path component is strictly
//! validated (`YYYY-MM-DD`) to defend against path traversal — a cache
//! key never comes from untrusted input here, but the guard is cheap
//! defense-in-depth, mirroring `instrument_snapshot`.

#![cfg(feature = "daily_universe_fetcher")]

use std::path::{Path, PathBuf};

use thiserror::Error;
use tickvault_common::constants::INDEX_CONSTITUENCY_CSV_CACHE_FILENAME;
use tickvault_common::instrument_types::IndexConstituencyMap;

/// Errors from the constituency cache.
#[derive(Debug, Error)]
pub enum ConstituencyCacheError {
    /// `trading_date` was not a strict `YYYY-MM-DD` (path-traversal guard).
    #[error("invalid trading-date cache key: {0}")]
    InvalidTradingDate(String),

    /// Filesystem I/O error.
    #[error("constituency cache I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization error.
    #[error("constituency cache JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Strict `YYYY-MM-DD` validation. Rejects any `/`, `.`, or non-digit
/// content so the value can never escape the cache base directory.
#[must_use]
pub fn is_valid_trading_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        let ok = match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// The cache file path for a trading date, after validating the date.
///
/// # Errors
///
/// [`ConstituencyCacheError::InvalidTradingDate`] if `trading_date` is
/// not a strict `YYYY-MM-DD`.
pub fn cache_path(base_dir: &Path, trading_date: &str) -> Result<PathBuf, ConstituencyCacheError> {
    if !is_valid_trading_date(trading_date) {
        return Err(ConstituencyCacheError::InvalidTradingDate(
            trading_date.to_string(),
        ));
    }
    Ok(base_dir
        .join(trading_date)
        .join(INDEX_CONSTITUENCY_CSV_CACHE_FILENAME))
}

/// Write the map as pretty JSON to the per-date cache file (creating
/// parent dirs). Writes to a temp file then renames for atomicity.
///
/// # Errors
///
/// See [`ConstituencyCacheError`].
pub fn write_cache(
    base_dir: &Path,
    trading_date: &str,
    map: &IndexConstituencyMap,
) -> Result<(), ConstituencyCacheError> {
    let path = cache_path(base_dir, trading_date)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(map)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the map from the per-date cache file. Returns `Ok(None)` if the
/// file does not exist (cold cache).
///
/// # Errors
///
/// See [`ConstituencyCacheError`].
pub fn read_cache(
    base_dir: &Path,
    trading_date: &str,
) -> Result<Option<IndexConstituencyMap>, ConstituencyCacheError> {
    let path = cache_path(base_dir, trading_date)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConstituencyCacheError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::instrument_types::IndexConstituent;

    fn sample_map() -> IndexConstituencyMap {
        let mut map = IndexConstituencyMap::default();
        let c = IndexConstituent {
            index_name: "Nifty Total Market".to_string(),
            symbol: "RELIANCE".to_string(),
            isin: "INE002A01018".to_string(),
            weight: 0.0,
            sector: "Oil Gas".to_string(),
            last_updated: chrono::NaiveDate::from_ymd_opt(2026, 6, 6).unwrap(),
        };
        map.index_to_constituents
            .insert("Nifty Total Market".to_string(), vec![c]);
        map.stock_to_indices.insert(
            "RELIANCE".to_string(),
            vec!["Nifty Total Market".to_string()],
        );
        map
    }

    #[test]
    fn round_trips_identity() {
        let dir = std::env::temp_dir().join(format!("tv-constituency-{}", std::process::id()));
        let map = sample_map();
        write_cache(&dir, "2026-06-06", &map).unwrap();
        let back = read_cache(&dir, "2026-06-06").unwrap().unwrap();
        assert_eq!(back.index_count(), 1);
        assert_eq!(back.stock_count(), 1);
        assert_eq!(
            back.get_constituents("Nifty Total Market").unwrap()[0].symbol,
            "RELIANCE"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_none() {
        let dir = std::env::temp_dir().join("tv-constituency-does-not-exist-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_cache(&dir, "2026-06-06").unwrap().is_none());
    }

    #[test]
    fn rejects_path_traversal_date() {
        let dir = std::env::temp_dir();
        for bad in [
            "../etc",
            "2026/06/06",
            "..",
            "2026-6-6",
            "2026-06-06x",
            "/abs",
        ] {
            assert!(cache_path(&dir, bad).is_err(), "{bad} should reject");
            assert!(!is_valid_trading_date(bad), "{bad}");
        }
        assert!(is_valid_trading_date("2026-06-06"));
    }
}
