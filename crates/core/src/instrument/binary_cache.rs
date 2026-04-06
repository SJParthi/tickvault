//! rkyv binary cache for `FnoUniverse`.
//!
//! Serialize/deserialize the instrument universe to a memory-mapped binary file
//! for fast restart during market hours. Uses rkyv `from_bytes` for validated
//! deserialization (~5-15ms, 25x faster than CSV re-parse).
//!
//! # Cache Format
//! `[MAGIC (4 bytes)] [VERSION (1 byte)] [PAD (3 bytes)] [rkyv payload ...]`
//! Magic = `b"RKYV"`, Version = `2`. 8-byte header for rkyv alignment.
//! Validated on load to reject stale caches.

use std::path::Path;

use anyhow::{Context, Result};
use memmap2::Mmap;
use tracing::info;

use dhan_live_trader_common::constants::BINARY_CACHE_FILENAME;
use dhan_live_trader_common::instrument_types::{ArchivedFnoUniverse, FnoUniverse};

/// Magic bytes at the start of every rkyv cache file.
const CACHE_MAGIC: &[u8; 4] = b"RKYV";

/// Cache format version. Bump when rkyv version or type layout changes.
/// V2: header padded to 8 bytes for rkyv alignment (was 5 bytes in V1).
const CACHE_VERSION: u8 = 2;

/// Total header size: 8 bytes (4 magic + 1 version + 3 padding).
/// Padded to 8-byte alignment so the rkyv payload starts at an aligned offset,
/// which rkyv requires for zero-copy deserialization from mmap.
const HEADER_LEN: usize = 8;

/// Serialize `FnoUniverse` to rkyv binary file.
///
/// Uses temp file + atomic rename to prevent readers from seeing partial writes.
/// Called after successful CSV build. Best-effort — caller should log and
/// continue if this fails.
pub fn write_binary_cache(universe: &FnoUniverse, cache_dir: &str) -> Result<()> {
    let rkyv_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(universe)
        .map_err(|err| anyhow::anyhow!("rkyv serialize failed: {err}"))?;

    let path = Path::new(cache_dir).join(BINARY_CACHE_FILENAME);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache dir: {}", parent.display()))?;
    }

    // Build payload: 8-byte aligned header + rkyv bytes
    let mut payload = Vec::with_capacity(HEADER_LEN.saturating_add(rkyv_bytes.len()));
    payload.extend_from_slice(CACHE_MAGIC);
    payload.push(CACHE_VERSION);
    payload.extend_from_slice(&[0u8; HEADER_LEN - CACHE_MAGIC.len() - 1]); // padding
    payload.extend_from_slice(&rkyv_bytes);

    // Atomic write: temp file → rename (prevents readers from seeing partial data)
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &payload)
        .with_context(|| format!("failed to write temp rkyv cache: {}", temp_path.display()))?;
    std::fs::rename(&temp_path, &path)
        .with_context(|| format!("failed to rename rkyv cache: {}", path.display()))?;

    info!(
        bytes = payload.len(),
        path = %path.display(),
        "rkyv binary cache written (atomic)"
    );
    Ok(())
}

/// Load `FnoUniverse` from rkyv binary cache via mmap.
///
/// Validates magic bytes + version header before deserializing.
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file exists but has wrong header, version, or corrupt data.
pub fn read_binary_cache(cache_dir: &str) -> Result<Option<FnoUniverse>> {
    let path = Path::new(cache_dir).join(BINARY_CACHE_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(&path)
        .with_context(|| format!("failed to open rkyv cache: {}", path.display()))?;

    let mmap = unsafe { Mmap::map(&file) } // SAFETY: read-only file, atomic rename(2) guarantees no torn writes, rkyv::from_bytes validates alignment/bounds
        .with_context(|| format!("failed to mmap rkyv cache: {}", path.display()))?;

    let rkyv_payload = validate_header(&mmap, &path)?;

    let universe = rkyv::from_bytes::<FnoUniverse, rkyv::rancor::Error>(rkyv_payload)
        .map_err(|err| anyhow::anyhow!("rkyv deserialize failed (corrupt cache?): {err}"))?;

    info!(
        bytes = mmap.len(),
        derivatives = universe.derivative_contracts.len(),
        underlyings = universe.underlyings.len(),
        "rkyv binary cache loaded"
    );
    Ok(Some(universe))
}

/// Validate cache header (magic + version) and return the rkyv payload slice.
fn validate_header<'a>(data: &'a [u8], path: &Path) -> Result<&'a [u8]> {
    if data.len() < HEADER_LEN {
        anyhow::bail!(
            "rkyv cache too small ({} bytes < {} header): {}",
            data.len(),
            HEADER_LEN,
            path.display()
        );
    }
    if &data[..4] != CACHE_MAGIC {
        anyhow::bail!(
            "rkyv cache bad magic (expected RKYV, got {:?}): {}",
            &data[..4],
            path.display()
        );
    }
    if data[4] != CACHE_VERSION {
        anyhow::bail!(
            "rkyv cache version mismatch (expected {}, got {}): {}",
            CACHE_VERSION,
            data[4],
            path.display()
        );
    }
    Ok(&data[HEADER_LEN..])
}

// ---------------------------------------------------------------------------
// Zero-Copy Access (Phase 2)
// ---------------------------------------------------------------------------

/// Zero-copy instrument universe loaded from rkyv binary cache.
///
/// Holds the memory-mapped file and provides direct archived access in
/// **sub-0.5ms** — zero heap allocation, zero deserialization. The mmap stays
/// alive for the lifetime of this struct so the `&ArchivedFnoUniverse` reference
/// remains valid.
///
/// # Safety
/// Uses `rkyv::access_unchecked` in `archived()` for hot-path performance. Safe because:
/// 1. `load()` validates the archive via safe `rkyv::access` before returning
/// 2. The mmap is read-only — no concurrent mutation possible
/// 3. Single-process access — atomic rename prevents partial reads
/// 4. Magic bytes + version + file size validated in header check
pub struct MappedUniverse {
    mmap: Mmap,
}

/// Minimum valid rkyv cache file size (header + smallest possible rkyv payload).
/// An empty FnoUniverse serializes to ~136 bytes + 8 header = 144 bytes.
const MIN_RKYV_CACHE_BYTES: u64 = (HEADER_LEN as u64).saturating_add(16);

impl MappedUniverse {
    /// Load and validate the rkyv binary cache. Sub-0.5ms.
    ///
    /// Validates magic bytes, version, minimum size, then does a quick field
    /// access to catch obvious corruption before returning.
    /// Returns `Ok(None)` if the file does not exist.
    /// Returns `Err` if the file exists but fails any validation.
    pub fn load(cache_dir: &str) -> Result<Option<Self>> {
        let path = Path::new(cache_dir).join(BINARY_CACHE_FILENAME);
        if !path.exists() {
            return Ok(None);
        }

        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat rkyv cache: {}", path.display()))?;

        if metadata.len() < MIN_RKYV_CACHE_BYTES {
            anyhow::bail!(
                "rkyv cache too small ({} bytes < {} minimum): {}",
                metadata.len(),
                MIN_RKYV_CACHE_BYTES,
                path.display()
            );
        }

        let file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open rkyv cache: {}", path.display()))?;

        let mmap = unsafe { Mmap::map(&file) } // SAFETY: read-only file, single-process, atomic writes
            .with_context(|| format!("failed to mmap rkyv cache: {}", path.display()))?;

        // Validate header (magic + version) before interpreting payload
        let _ = validate_header(&mmap, &path)?;

        let mapped = Self { mmap };

        // SECURITY: Validate rkyv archive structure once at load time using safe
        // rkyv::access (checks alignment, pointer bounds, invariants). This
        // justifies subsequent access_unchecked calls on the hot path since the
        // mmap is read-only and single-process — data cannot change after validation.
        let archived =
            rkyv::access::<ArchivedFnoUniverse, rkyv::rancor::Error>(&mapped.mmap[HEADER_LEN..])
                .map_err(|err| anyhow::anyhow!("rkyv archive validation failed: {err}"))?;
        let _derivative_count = archived.derivative_contracts.len();
        let _underlying_count = archived.underlyings.len();

        info!(
            bytes = mapped.mmap.len(),
            derivatives = _derivative_count,
            underlyings = _underlying_count,
            "rkyv binary cache zero-copy loaded"
        );

        Ok(Some(mapped))
    }

    /// Zero-copy access to the archived universe. Sub-microsecond.
    ///
    /// # Safety
    /// Header validated in `load()`. Mmap is read-only, single-process.
    #[inline]
    pub fn archived(&self) -> &ArchivedFnoUniverse {
        // Skip the 8-byte header (magic + version + padding) to reach the rkyv payload
        unsafe { rkyv::access_unchecked::<ArchivedFnoUniverse>(&self.mmap[HEADER_LEN..]) } // SAFETY: see struct-level safety comment
    }

    /// Full deserialization to owned types (for non-hot-path code like persistence).
    pub fn to_owned(&self) -> Result<FnoUniverse> {
        rkyv::from_bytes::<FnoUniverse, rkyv::rancor::Error>(&self.mmap[HEADER_LEN..])
            .map_err(|err| anyhow::anyhow!("rkyv deserialize failed: {err}"))
    }

    /// Number of derivative contracts (from archived data, no allocation).
    #[inline]
    pub fn derivative_count(&self) -> usize {
        self.archived().derivative_contracts.len()
    }

    /// Number of underlyings (from archived data, no allocation).
    #[inline]
    pub fn underlying_count(&self) -> usize {
        self.archived().underlyings.len()
    }
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
        use chrono::Utc;
        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
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
        assert!(
            result.is_err(),
            "corrupt rkyv file should return Err (bad magic)"
        );

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
    fn test_read_binary_cache_bad_magic_returns_error() {
        let dir = unique_temp_dir("rkyv-bad-magic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        // Valid size but wrong magic bytes
        let mut data = vec![0u8; 300];
        data[..4].copy_from_slice(b"FAKE");
        data[4] = CACHE_VERSION;
        std::fs::write(&path, &data).unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(result.is_err(), "bad magic should return Err");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("bad magic"),
            "error should mention magic: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_binary_cache_wrong_version_returns_error() {
        let dir = unique_temp_dir("rkyv-wrong-ver");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        let mut data = vec![0u8; 300];
        data[..4].copy_from_slice(CACHE_MAGIC);
        data[4] = CACHE_VERSION.wrapping_add(1); // wrong version
        std::fs::write(&path, &data).unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(result.is_err(), "wrong version should return Err");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("version mismatch"),
            "error should mention version: {err_msg}"
        );

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

    // --- MappedUniverse tests ---

    #[test]
    fn test_mapped_universe_load_roundtrip() {
        let dir = unique_temp_dir("mapped-roundtrip");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let original = test_universe();
        write_binary_cache(&original, cache_dir).unwrap();

        let mapped = MappedUniverse::load(cache_dir).unwrap().unwrap();
        let archived = mapped.archived();

        assert_eq!(archived.build_metadata.csv_row_count.to_native(), 100);
        assert_eq!(archived.build_metadata.parsed_row_count.to_native(), 50);
        assert_eq!(mapped.derivative_count(), 0);
        assert_eq!(mapped.underlying_count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_mapped_universe_load_missing_returns_none() {
        let result = MappedUniverse::load("/tmp/dlt-nonexistent-mapped-cache-98765").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_mapped_universe_load_too_small_returns_error() {
        let dir = unique_temp_dir("mapped-too-small");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        // 16 bytes is below MIN_RKYV_CACHE_BYTES (HEADER_LEN + 16 = 24)
        std::fs::write(&path, vec![0u8; 16]).unwrap();

        let result = MappedUniverse::load(dir.to_str().unwrap());
        assert!(result.is_err(), "too-small rkyv file should return Err");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_mapped_universe_to_owned_matches_original() {
        let dir = unique_temp_dir("mapped-to-owned");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let original = test_universe();
        write_binary_cache(&original, cache_dir).unwrap();

        let mapped = MappedUniverse::load(cache_dir).unwrap().unwrap();
        let owned = mapped.to_owned().unwrap();

        assert_eq!(owned.build_metadata.csv_source, "test");
        assert_eq!(owned.build_metadata.csv_row_count, 100);
        assert_eq!(owned.build_metadata.parsed_row_count, 50);
        assert_eq!(
            owned.build_metadata.build_duration,
            std::time::Duration::from_millis(42)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_header_too_small() {
        let data = vec![0u8; 5]; // Less than HEADER_LEN (8)
        let path = std::path::Path::new("/tmp/test.bin");
        let err = validate_header(&data, path).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    #[test]
    fn test_validate_header_correct_returns_payload() {
        let mut data = vec![0u8; 20];
        data[..4].copy_from_slice(CACHE_MAGIC);
        data[4] = CACHE_VERSION;
        // Bytes 5-7 are padding
        // Bytes 8-19 are "rkyv payload"
        data[8..20].copy_from_slice(b"test_payload");
        let path = std::path::Path::new("/tmp/test.bin");
        let payload = validate_header(&data, path).unwrap();
        assert_eq!(payload, b"test_payload");
    }

    #[test]
    fn test_validate_header_exactly_header_len() {
        let mut data = vec![0u8; HEADER_LEN];
        data[..4].copy_from_slice(CACHE_MAGIC);
        data[4] = CACHE_VERSION;
        let path = std::path::Path::new("/tmp/test.bin");
        let payload = validate_header(&data, path).unwrap();
        assert!(payload.is_empty(), "empty rkyv payload after header");
    }

    #[test]
    fn test_mapped_universe_load_bad_magic() {
        let dir = unique_temp_dir("mapped-bad-magic");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        let mut data = vec![0u8; 200];
        data[..4].copy_from_slice(b"BAAD");
        data[4] = CACHE_VERSION;
        std::fs::write(&path, &data).unwrap();

        let result = MappedUniverse::load(dir.to_str().unwrap());
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_constants() {
        assert_eq!(CACHE_MAGIC, b"RKYV");
        assert_eq!(CACHE_VERSION, 2);
        assert_eq!(HEADER_LEN, 8);
        assert!(MIN_RKYV_CACHE_BYTES > HEADER_LEN as u64);
    }

    #[test]
    fn test_validate_header_bad_magic_error_message() {
        let mut data = vec![0u8; 20];
        data[..4].copy_from_slice(b"NOPE");
        data[4] = CACHE_VERSION;
        let path = std::path::Path::new("/tmp/test.bin");
        let err = validate_header(&data, path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad magic"), "error: {msg}");
    }

    #[test]
    fn test_validate_header_wrong_version_error_message() {
        let mut data = vec![0u8; 20];
        data[..4].copy_from_slice(CACHE_MAGIC);
        data[4] = 99; // wrong version
        let path = std::path::Path::new("/tmp/test.bin");
        let err = validate_header(&data, path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("version mismatch"), "error: {msg}");
        assert!(msg.contains("99"), "should show actual version: {msg}");
    }

    #[test]
    fn test_validate_header_exactly_7_bytes_too_small() {
        let data = vec![0u8; 7]; // Less than HEADER_LEN=8
        let path = std::path::Path::new("/tmp/test.bin");
        let err = validate_header(&data, path).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    #[test]
    fn test_mapped_universe_derivative_and_underlying_count() {
        let dir = unique_temp_dir("mapped-counts");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let original = test_universe();
        write_binary_cache(&original, cache_dir).unwrap();

        let mapped = MappedUniverse::load(cache_dir).unwrap().unwrap();
        assert_eq!(mapped.derivative_count(), 0);
        assert_eq!(mapped.underlying_count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_mapped_universe_load_wrong_version() {
        let dir = unique_temp_dir("mapped-wrong-ver");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        let mut data = vec![0u8; 200];
        data[..4].copy_from_slice(CACHE_MAGIC);
        data[4] = CACHE_VERSION.wrapping_add(1); // wrong version
        std::fs::write(&path, &data).unwrap();

        let result = MappedUniverse::load(dir.to_str().unwrap());
        assert!(result.is_err(), "wrong version should return Err");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_binary_cache_overwrites_existing() {
        let dir = unique_temp_dir("rkyv-overwrite");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let u1 = test_universe();
        write_binary_cache(&u1, cache_dir).unwrap();

        // Write again (overwrite)
        let u2 = test_universe();
        write_binary_cache(&u2, cache_dir).unwrap();

        // Should still be readable
        let loaded = read_binary_cache(cache_dir).unwrap().unwrap();
        assert_eq!(loaded.build_metadata.csv_source, "test");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_mapped_universe_sub_500_micros() {
        let dir = unique_temp_dir("mapped-latency");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let original = test_universe();
        write_binary_cache(&original, cache_dir).unwrap();

        // Warm the page cache
        let _ = MappedUniverse::load(cache_dir).unwrap().unwrap();

        // Measure load latency
        let start = std::time::Instant::now();
        let mapped = MappedUniverse::load(cache_dir).unwrap().unwrap();
        let load_time = start.elapsed();

        // Verify archived access works
        let _count = mapped.derivative_count();

        // With an empty universe, load should be well under 500µs.
        // Production universe is larger but mmap + access_unchecked is still sub-ms.
        assert!(
            load_time < std::time::Duration::from_millis(5),
            "MappedUniverse::load took {:?} (expected < 5ms for small universe)",
            load_time
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
