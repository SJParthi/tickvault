//! Ratchet: instrument + CSV cache writes MUST fsync + atomically rename.
//!
//! # Why (queue items I2 + I3, 2026-04-21)
//!
//! Live production log at 13:06:02 IST today:
//! ```
//! level=ERROR "CRITICAL: no instrument cache available during market hours
//!   — triggering emergency download"
//! ```
//! Followed by a successful download of 106,214 derivatives. The previous
//! run MUST have written a cache file, but the cache was gone on restart.
//!
//! Root cause: `std::fs::write` + `std::fs::rename` persist via OS page
//! cache only. A crash / power cycle / container SIGKILL before the
//! kernel flushes the page cache → zero-length or missing cache file
//! → I-P0-06 emergency download fires on next boot (cost: 6-10 s of
//! boot + one network round-trip that competes with the market-open
//! 09:15 IST deadline).
//!
//! Fix (this PR): both `binary_cache::write_binary_cache` and
//! `csv_downloader::write_cache` now (a) write to a temp file,
//! (b) `sync_all()` the file, (c) atomically rename over the final
//! path, (d) `sync_all()` the parent directory. Matches the
//! "write → rename → fsync(dir)" pattern used by SQLite, PostgreSQL,
//! and every serious durability-sensitive writer.
//!
//! These ratchets scan the source so the durability pattern can't
//! silently regress back to plain `fs::write`.

use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// I2: rkyv binary cache
// ---------------------------------------------------------------------------

#[test]
fn binary_cache_write_fsyncs_temp_file_before_rename() {
    let src = read("crates/core/src/instrument/binary_cache.rs");
    assert!(
        src.contains("file.sync_all()"),
        "binary_cache.rs::write_binary_cache MUST call `sync_all()` on the \
         temp file BEFORE the rename. Without this, a crash between write \
         and kernel flush leaves a zero-length cache file and the next \
         boot hits I-P0-06 emergency download."
    );
}

#[test]
fn binary_cache_write_fsyncs_parent_directory_after_rename() {
    let src = read("crates/core/src/instrument/binary_cache.rs");
    assert!(
        src.contains("dir.sync_all()"),
        "binary_cache.rs::write_binary_cache MUST fsync the parent \
         directory AFTER the rename so the directory-entry update is \
         persisted. Without this, a crash after rename but before dir-inode \
         flush leaves the old cache pointer visible and the new data \
         invisible on reboot."
    );
}

#[test]
fn binary_cache_write_uses_atomic_rename() {
    let src = read("crates/core/src/instrument/binary_cache.rs");
    assert!(
        src.contains("std::fs::rename(&temp_path, &path)"),
        "binary_cache.rs MUST use atomic rename (temp → final) so readers \
         never observe a half-written file."
    );
}

// ---------------------------------------------------------------------------
// I3: CSV cache (async path)
// ---------------------------------------------------------------------------

#[test]
fn csv_cache_write_goes_through_durable_helper() {
    let src = read("crates/core/src/instrument/csv_downloader.rs");
    assert!(
        src.contains("async fn write_csv_durable"),
        "csv_downloader.rs MUST expose a `write_csv_durable` helper — \
         refactored in 2026-04-21 fix for I3 so the write path is \
         testable + has one fsync implementation to review."
    );
    assert!(
        src.contains("file.sync_all().await"),
        "csv_downloader.rs::write_csv_durable MUST fsync the temp file."
    );
    assert!(
        src.contains("dir.sync_all().await"),
        "csv_downloader.rs::write_csv_durable MUST fsync the parent directory."
    );
    assert!(
        src.contains("fs::rename(&tmp_path, cache_path)"),
        "csv_downloader.rs MUST use atomic rename (temp → final)."
    );
}

#[test]
fn csv_cache_no_longer_calls_plain_fs_write() {
    let src = read("crates/core/src/instrument/csv_downloader.rs");
    // The old `fs::write(&cache_path, csv_text)` line must be gone.
    // `fs::write(` is still allowed elsewhere (e.g., freshness marker)
    // but NOT on `cache_path` directly.
    assert!(
        !src.contains("fs::write(&cache_path, csv_text)"),
        "csv_downloader.rs MUST NOT call `fs::write(&cache_path, csv_text)` \
         directly — must go through `write_csv_durable` so the fsync path \
         is exercised."
    );
}
