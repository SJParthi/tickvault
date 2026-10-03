//! Raw WAL segments to the cold bucket, verified, before any local delete
//! (plan item 45e-1, operator Quotes 27 + 28, 2026-09-29).
//!
//! # What this does
//!
//! Every sealed capture segment (`ws-frames-<nanos>.wal`, never the one the
//! writer holds open) in the WAL root, `replaying/` and `archive/` is:
//!
//! 1. read in 1 MiB chunks, hashed (SHA-256) and gzipped (level 1) into a
//!    pre-sized in-memory buffer — in memory on purpose, so a full disk cannot
//!    block the upload with a temp file;
//! 2. round-trip checked: the gzip is decompressed and must hash back to the
//!    raw SHA-256 and length;
//! 3. uploaded to `raw-frames/<IST date>/<segment name>.gz` with
//!    `If-None-Match: *` (never an overwrite) and the gzip's SHA-256 as the S3
//!    checksum, so S3 rejects a corrupted body;
//! 4. verified with HeadObject: the stored length and SHA-256 must equal the
//!    gzip's. Any mismatch writes NO marker;
//! 5. recorded in a marker file `<wal_dir>/uploaded/<segment name>` (tmp,
//!    fsync, rename, directory fsync) holding the key, both digests and both
//!    lengths.
//!
//! An object already at the key with the same length and SHA-256 is reused
//! (a crash between upload and marker). A different object is never touched:
//! the bytes go to a content-addressed sidecar key instead
//! (`<key minus .gz>.sha-<gzip sha hex>.gz`), the same rule
//! `partition_archive` uses.
//!
//! # Who reads the marker
//!
//! `ws_frame_spill`'s WAL prune. With `[raw_frame_archive]
//! require_upload_before_prune` on (the default), every delete pass — age,
//! byte ceiling and the 5% hard floor — needs the segment's marker AND the
//! marker's recorded raw length must equal the file's length now. The marker
//! is keyed by file NAME, so it follows a segment moved between the WAL root,
//! `replaying/` and `archive/` by rename. Segments are append-only and never
//! rewritten once sealed, so name plus length is enough without re-hashing.
//!
//! # Where it runs
//!
//! Its own tokio task in `main.rs`: every [`RAW_UPLOAD_INTERVAL_SECS`] outside
//! 09:00–15:40 IST, and at once on disk pressure (at most
//! [`RAW_UPLOAD_PRESSURE_BATCH`] segments per pressure wake inside the
//! session). The read, gzip and hashing run in `spawn_blocking` — never on
//! the frame drain or the WAL writer thread. Cold path throughout.
//!
//! # Honest limits
//!
//! - This step does NOT break a deadlock where the stuck segments are
//!   UNAPPLIED: the prune still refuses those. Offloading uploaded unapplied
//!   segments and re-hydrating them after close is plan item 45e-2, not built
//!   here.
//! - A segment the writer created but has not yet registered as open (a
//!   window of a few instructions at rotation) is excluded by the
//!   [`RAW_UPLOAD_MIN_QUIET_SECS`] mtime guard, not by the registry. If one
//!   were uploaded early anyway, its length would later differ from the
//!   marker's, the prune would refuse it, and the next pass would upload it
//!   again to the sidecar key.
//! - A crash between a segment's delete and its marker's delete leaves an
//!   orphan marker (a few hundred bytes). Nothing reads it.

use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64;
use tickvault_common::error_code::ErrorCode;

use crate::s3_cold::{
    ExistingObject, S3Cold, S3ObjectMeta, Sha256Writer, base64_encode, classify_existing,
    hex_encode,
};

/// Key prefix inside the cold bucket. The terraform lifecycle moves this
/// prefix to Deep Archive after 30 days and never expires it (item 45f).
pub const RAW_FRAME_S3_PREFIX: &str = "raw-frames";

/// Marker directory under the WAL root (and, for the other file sets, under
/// each directory they cover).
pub const UPLOADED_SUBDIR: &str = "uploaded";

/// Key prefix for sealed-candle spill files (`data/spill`, `replaying/`,
/// `archive/`).
pub const SEAL_SPILL_S3_PREFIX: &str = "seal-spill";
/// Key prefix for quarantined tick spill files.
pub const TICK_QUARANTINE_S3_PREFIX: &str = "tick-quarantine";
/// Key prefix for quarantined depth spill files.
pub const DEPTH_QUARANTINE_S3_PREFIX: &str = "depth-quarantine";

/// Counter: spill / quarantine files uploaded (or found uploaded) and
/// marked, labelled `set`.
pub const COLD_FILE_UPLOAD_OK_COUNTER: &str = "tv_cold_file_upload_ok_total";
/// Counter: spill / quarantine file attempts that ended without a marker,
/// labelled `set`.
pub const COLD_FILE_UPLOAD_FAILED_COUNTER: &str = "tv_cold_file_upload_failed_total";
/// Gauge: spill / quarantine files still without a valid marker after the
/// last pass, labelled `set`.
pub const COLD_FILE_UPLOAD_BACKLOG_GAUGE: &str = "tv_cold_file_upload_backlog_files";

/// Scheduled cadence of the upload pass outside the session window.
pub const RAW_UPLOAD_INTERVAL_SECS: u64 = 120;

/// Most segments one pressure wake uploads INSIDE the session window, so a
/// pressure episode at 11:00 does not start a multi-gigabyte upload beside
/// the live feed. Outside the window a pass runs to the end of the backlog.
pub const RAW_UPLOAD_PRESSURE_BATCH: usize = 4;

/// A segment whose mtime is newer than this is not touched yet (see the
/// rotation note in the module docs).
pub const RAW_UPLOAD_MIN_QUIET_SECS: u64 = 60;

/// Consecutive failed segments after which a pass stops: S3 is most likely
/// unreachable, and hammering it per segment helps nobody.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// Pre-size of the gzip buffer: 128 MiB segments compress to roughly half.
const GZIP_PRESIZE_BYTES: usize = 64 * 1024 * 1024;

/// Read chunk for hashing and compressing a segment.
const READ_CHUNK_BYTES: usize = 1024 * 1024;

/// Marker format version.
const MARKER_VERSION: u32 = 1;

/// Session window (IST seconds of day) in which the scheduled pass does not
/// run: 09:00 to 15:40.
const SESSION_START_SECS_IST: i64 = 9 * 3_600;
const SESSION_END_SECS_IST: i64 = 15 * 3_600 + 40 * 60;

/// Counter: segments uploaded (or found already uploaded) and marked.
pub const RAW_UPLOAD_OK_COUNTER: &str = "tv_raw_frame_upload_ok_total";
/// Counter: segment attempts that ended without a marker.
pub const RAW_UPLOAD_FAILED_COUNTER: &str = "tv_raw_frame_upload_failed_total";
/// Gauge: sealed segments still without a valid marker after the last pass.
pub const RAW_UPLOAD_BACKLOG_GAUGE: &str = "tv_raw_frame_upload_backlog_segments";

/// The two S3 calls the uploader makes, behind a trait so its logic is
/// testable without AWS.
pub trait ColdObjectStore: Send + Sync {
    /// The bucket name, for the marker and the logs.
    fn bucket_name(&self) -> &str;

    /// HeadObject: `Ok(None)` only when the key does not exist.
    fn head_object(&self, key: &str) -> impl Future<Output = Result<Option<S3ObjectMeta>>> + Send;

    /// Conditional create (`If-None-Match: *`) with the SHA-256 checksum.
    fn put_object_if_absent(
        &self,
        key: &str,
        body: Vec<u8>,
        sha256_b64: &str,
        metadata: &[(&'static str, String)],
    ) -> impl Future<Output = Result<()>> + Send;
}

impl ColdObjectStore for S3Cold {
    fn bucket_name(&self) -> &str {
        self.bucket()
    }

    async fn head_object(&self, key: &str) -> Result<Option<S3ObjectMeta>> {
        self.head(key).await
    }

    async fn put_object_if_absent(
        &self,
        key: &str,
        body: Vec<u8>,
        sha256_b64: &str,
        metadata: &[(&'static str, String)],
    ) -> Result<()> {
        self.put_if_absent_bytes(key, body, sha256_b64, metadata)
            .await
    }
}

/// The record that a segment's bytes are in the cold bucket, verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadMarker {
    pub version: u32,
    pub bucket: String,
    pub key: String,
    /// SHA-256 of the gzip object, lowercase hex.
    pub gzip_sha256: String,
    pub gzip_len: u64,
    /// SHA-256 of the raw segment, lowercase hex.
    pub raw_sha256: String,
    pub raw_len: u64,
    /// Raw segment mtime, whole seconds since the epoch.
    pub raw_mtime_secs: u64,
    /// First frame sequence of a WAL segment (plan item 45e-2), so a deleted
    /// segment can still be placed in capture order. `None` for other files,
    /// for markers written before it existed, and for an unreadable first
    /// record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_frame_seq: Option<u64>,
}

/// `<wal_dir>/uploaded`.
#[must_use]
pub fn markers_dir(wal_dir: &Path) -> PathBuf {
    wal_dir.join(UPLOADED_SUBDIR)
}

/// Most verified uploads this process keeps in memory while their marker
/// file cannot be written (S2). Past it a verified upload is not recorded and
/// its file is uploaded again on a later pass, so the bound costs work, never
/// a delete.
pub const UNMARKED_VERIFIED_LIMIT: usize = 4096;

/// Gauge: verified uploads held in memory because their marker could not be
/// written (S2). Non-zero means the disk refused a marker write.
pub const UNMARKED_VERIFIED_GAUGE: &str = "tv_raw_upload_unmarked_verified";

/// Counter: marker writes that failed after a verified upload (S2).
pub const MARKER_WRITE_FAILED_COUNTER: &str = "tv_raw_upload_marker_write_failed_total";

/// S2 (2026-10-02): verified uploads THIS process made whose marker file could
/// not be written, keyed by the marker's path.
///
/// The defect it closes: on a 100%-full disk the upload and its HeadObject
/// check succeed, the marker write fails with ENOSPC, and with no marker every
/// prune that requires one refuses forever, so the disk never frees. An entry
/// is added ONLY after the same HeadObject verification a marker records, and
/// carries the same marker body, so it satisfies a prune exactly when the
/// marker would have (raw length, and mtime for the spill and quarantine
/// sets). It is never read from disk, so another process's claim cannot
/// enter it. A later pass retries the marker write and drops the entry once
/// the marker is on disk; a prune that deletes the file drops it too.
///
/// One `HashMap` probe per lookup under a `Mutex`, at most
/// [`UNMARKED_VERIFIED_LIMIT`] entries. Cold paths only (the upload pass and
/// the prunes), never the frame drain.
static UNMARKED_VERIFIED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, UploadMarker>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn unmarked_verified()
-> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, UploadMarker>> {
    UNMARKED_VERIFIED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records a verified upload whose marker write failed. `false` when the
/// record is full (the file is then simply uploaded again later).
fn remember_unmarked(marker_path: PathBuf, marker: UploadMarker) -> bool {
    let mut held = unmarked_verified();
    if held.len() >= UNMARKED_VERIFIED_LIMIT && !held.contains_key(&marker_path) {
        return false;
    }
    held.insert(marker_path, marker);
    // APPROVED: cast — an entry count bounded at 4096.
    #[allow(clippy::cast_precision_loss)] // APPROVED: bounded entry count
    metrics::gauge!(UNMARKED_VERIFIED_GAUGE).set(held.len() as f64);
    true
}

/// Drops the in-memory record for a marker path (marker written, or the
/// file deleted).
fn forget_unmarked(marker_path: &Path) {
    let mut held = unmarked_verified();
    if held.remove(marker_path).is_some() {
        // APPROVED: cast — an entry count bounded at 4096.
        #[allow(clippy::cast_precision_loss)] // APPROVED: bounded entry count
        metrics::gauge!(UNMARKED_VERIFIED_GAUGE).set(held.len() as f64);
    }
}

/// Retries the marker write of every verified upload held in memory and drops
/// each one that lands. Returns how many are still held. Cold: at the start of
/// each upload pass, at most [`UNMARKED_VERIFIED_LIMIT`] small writes.
fn retry_unmarked_markers() -> usize {
    // O(1) EXEMPT: begin — at most UNMARKED_VERIFIED_LIMIT entries, cold upload pass
    let snapshot: Vec<(PathBuf, UploadMarker)> = unmarked_verified()
        .iter()
        .map(|(path, marker)| (path.clone(), marker.clone()))
        .collect();
    for (path, marker) in snapshot {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            continue;
        };
        if write_marker(dir, name, &marker).is_ok() {
            forget_unmarked(&path);
        }
    }
    // O(1) EXEMPT: end
    unmarked_verified().len()
}

/// [`retry_unmarked_markers`] off the async runtime, logging what is still
/// held. Skips the blocking task entirely while nothing is held.
async fn retry_held_markers() {
    if unmarked_verified().is_empty() {
        return;
    }
    if let Ok(still_held) = tokio::task::spawn_blocking(retry_unmarked_markers).await
        && still_held > 0
    {
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            source = "raw_frame_upload_marker",
            still_held,
            "verified copies in S3 still have no marker on disk; held in memory and \
             retried on the next pass"
        );
    }
}

/// Reads the marker for `segment_name`, `None` when absent or unreadable.
/// Falls back to a verified upload this process holds in memory because its
/// marker could not be written (S2).
fn read_marker(markers_dir: &Path, segment_name: &str) -> Option<UploadMarker> {
    let path = markers_dir.join(segment_name);
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<UploadMarker>(&bytes).ok())
        .or_else(|| unmarked_verified().get(&path).cloned())
}

/// Whether `segment` has a marker whose recorded raw length equals `len`.
/// What the WAL prune asks before every delete. Cold path: one small file
/// read per segment per prune pass.
#[must_use]
pub fn marker_matches(markers_dir: &Path, segment: &Path, len: u64) -> bool {
    segment
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| read_marker(markers_dir, name))
        .is_some_and(|m| m.raw_len == len)
}

/// Whole seconds since the epoch of a file's mtime, `0` when unreadable —
/// the same reading [`prepare_segment`] records as `raw_mtime_secs`.
fn mtime_secs_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Whether `path` has a marker in `markers_dir` whose recorded raw length
/// AND mtime equal `len` and `mtime_secs`. The spill and quarantine prunes
/// ask this: their file names repeat across days and folders (a day file is
/// recreated after the replay stages it), so the name and length alone could
/// match a different file. Cold path: one small file read.
#[must_use]
pub fn marker_matches_file(markers_dir: &Path, path: &Path, len: u64, mtime_secs: u64) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| read_marker(markers_dir, name))
        .is_some_and(|m| m.raw_len == len && m.raw_mtime_secs == mtime_secs)
}

/// `<dir>/uploaded` for a file in `dir`: where the spill and quarantine sets
/// keep a file's marker. `None` for a path with no parent.
#[must_use]
pub fn sibling_markers_dir(path: &Path) -> Option<PathBuf> {
    path.parent().map(|p| p.join(UPLOADED_SUBDIR))
}

/// Whether a spill or quarantine prune may delete a file (operator Quotes 27
/// + 28: no market-data delete without a verified copy). Config:
/// `[raw_frame_archive] require_upload_before_prune`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyGate {
    /// No copy needed: the behaviour before this gate. A box without a bucket
    /// and with the config off uses this.
    NotRequired,
    /// Every delete needs the file's marker in `<its dir>/uploaded/`, with
    /// the file's current length and mtime.
    Required,
}

impl CopyGate {
    /// The gate for `require_upload_before_prune`.
    #[must_use]
    pub const fn from_config(require_upload: bool) -> Self {
        if require_upload {
            Self::Required
        } else {
            Self::NotRequired
        }
    }

    /// Whether `path`, whose metadata is `meta`, may be deleted as far as the
    /// copy is concerned. Cold prune path: one small marker read.
    #[must_use]
    pub fn allows_delete(self, path: &Path, meta: &std::fs::Metadata) -> bool {
        match self {
            Self::NotRequired => true,
            Self::Required => sibling_markers_dir(path).is_some_and(|markers| {
                marker_matches_file(&markers, path, meta.len(), mtime_secs_of(meta))
            }),
        }
    }

    /// Whether a delete under this gate leaves a verified copy in S3.
    #[must_use]
    pub const fn copy_in_s3(self) -> bool {
        matches!(self, Self::Required)
    }

    /// Removes the marker of a file the prune just deleted. Best effort: a
    /// leftover marker never matches another file (length and mtime).
    pub fn forget(self, path: &Path) {
        if let (Self::Required, Some(markers)) = (self, sibling_markers_dir(path)) {
            remove_marker(&markers, path);
        }
    }
}

/// Removes `segment`'s marker after the prune deleted the segment.
/// Best effort: a leftover marker is harmless (see the module docs).
pub fn remove_marker(markers_dir: &Path, segment: &Path) {
    if let Some(name) = segment.file_name() {
        // O(1) EXEMPT: cold prune path, one unlink after a segment unlink
        drop(std::fs::remove_file(markers_dir.join(name)));
        forget_unmarked(&markers_dir.join(name));
    }
}

/// Writes the marker durably: tmp file, fsync, rename, directory fsync.
fn write_marker(markers_dir: &Path, segment_name: &str, marker: &UploadMarker) -> Result<()> {
    std::fs::create_dir_all(markers_dir).context("create the upload marker directory")?;
    let body = serde_json::to_vec(marker).context("serialise the upload marker")?;
    let tmp = markers_dir.join(format!("{segment_name}.tmp"));
    let dst = markers_dir.join(segment_name);
    {
        let mut f = std::fs::File::create(&tmp).context("create the marker tmp file")?;
        f.write_all(&body).context("write the marker tmp file")?;
        f.sync_all().context("fsync the marker tmp file")?;
    }
    std::fs::rename(&tmp, &dst).context("rename the marker into place")?;
    std::fs::File::open(markers_dir)
        .and_then(|d| d.sync_all())
        .context("fsync the marker directory")?;
    Ok(())
}

/// IST calendar date (`YYYY-MM-DD`) of a UTC epoch second.
fn ist_date(utc_secs: i64) -> String {
    chrono::DateTime::from_timestamp(utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64), 0)
        .map_or_else(
            || "undated".to_string(),
            |d| d.date_naive().format("%Y-%m-%d").to_string(),
        )
}

/// The canonical key for a segment: `raw-frames/<IST date>/<name>.gz`. The
/// date comes from the nanos in `ws-frames-<nanos>.wal`, or from the mtime
/// when the name carries none.
#[must_use]
pub fn segment_key(segment_name: &str, mtime_secs: u64) -> String {
    let nanos = segment_name
        .strip_prefix("ws-frames-")
        .and_then(|s| s.strip_suffix(".wal"))
        .and_then(|s| s.parse::<u128>().ok());
    let secs = nanos.map_or_else(
        || i64::try_from(mtime_secs).unwrap_or(i64::MAX),
        |n| i64::try_from(n / 1_000_000_000).unwrap_or(i64::MAX),
    );
    dated_key(RAW_FRAME_S3_PREFIX, secs, segment_name)
}

/// `<prefix>/<IST date of utc_secs>/<name>.gz`.
fn dated_key(prefix: &str, utc_secs: i64, name: &str) -> String {
    format!("{prefix}/{}/{name}.gz", ist_date(utc_secs))
}

/// The canonical key for a spill or quarantine file:
/// `<prefix>/<IST date of its mtime>/<name>.gz`.
#[must_use]
pub fn cold_file_key(prefix: &str, name: &str, mtime_secs: u64) -> String {
    dated_key(prefix, i64::try_from(mtime_secs).unwrap_or(i64::MAX), name)
}

/// The content-addressed key used when the canonical key holds different
/// bytes. Collision-free (the full digest is in the name) and idempotent.
fn sidecar_key(canonical: &str, gzip_sha256_hex: &str) -> String {
    let stem = canonical.strip_suffix(".gz").unwrap_or(canonical);
    format!("{stem}.sha-{gzip_sha256_hex}.gz")
}

/// Whether the scheduled pass may run: outside 09:00–15:40 IST.
#[must_use]
pub fn upload_window_open(utc_secs: i64) -> bool {
    let ist_secs_of_day = utc_secs
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .rem_euclid(86_400);
    !(SESSION_START_SECS_IST..SESSION_END_SECS_IST).contains(&ist_secs_of_day)
}

/// A segment read, hashed, compressed and round-trip checked.
struct PreparedSegment {
    raw_len: u64,
    raw_sha256_hex: String,
    raw_mtime_secs: u64,
    gzip: Vec<u8>,
    gzip_sha256_b64: String,
    gzip_sha256_hex: String,
}

/// Blocking: reads, hashes and gzips `path`, then decompresses the gzip and
/// checks it hashes back to the same bytes. Errors if the file changed length
/// while it was read. Runs inside `spawn_blocking`.
fn prepare_segment(path: &Path) -> Result<PreparedSegment> {
    use sha2::Digest as _;
    let mut file = std::fs::File::open(path).context("open the segment")?;
    let meta = file.metadata().context("stat the segment")?;
    let raw_mtime_secs = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut raw_hasher = sha2::Sha256::new();
    // A spill or quarantine file is often a few KiB; pre-size to the file,
    // capped at the segment-sized figure.
    let presize = usize::try_from(meta.len())
        .unwrap_or(GZIP_PRESIZE_BYTES)
        .min(GZIP_PRESIZE_BYTES);
    let mut encoder = GzEncoder::new(
        Sha256Writer::new(Vec::with_capacity(presize)),
        Compression::fast(),
    );
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    let mut raw_len: u64 = 0;
    loop {
        let n = file.read(&mut chunk).context("read the segment")?;
        if n == 0 {
            break;
        }
        raw_hasher.update(&chunk[..n]);
        encoder.write_all(&chunk[..n]).context("gzip the segment")?;
        raw_len = raw_len.saturating_add(n as u64);
    }
    let (gzip, gzip_digest) = encoder.finish().context("finish the gzip")?.finish();
    let raw_digest: [u8; 32] = raw_hasher.finalize().into();

    // Round trip: the object must decompress to exactly the segment.
    let mut check_hasher = sha2::Sha256::new();
    let mut decoder = GzDecoder::new(gzip.as_slice());
    let mut check_len: u64 = 0;
    loop {
        let n = decoder
            .read(&mut chunk)
            .context("decompress the gzip for the round-trip check")?;
        if n == 0 {
            break;
        }
        check_hasher.update(&chunk[..n]);
        check_len = check_len.saturating_add(n as u64);
    }
    let check_digest: [u8; 32] = check_hasher.finalize().into();
    if check_digest != raw_digest || check_len != raw_len {
        anyhow::bail!(
            "gzip round trip does not reproduce the segment ({check_len} bytes back, {raw_len} read)"
        );
    }
    let len_now = std::fs::metadata(path)
        .context("re-stat the segment")?
        .len();
    if len_now != raw_len {
        anyhow::bail!("segment changed length while it was read ({raw_len} -> {len_now})");
    }
    Ok(PreparedSegment {
        raw_len,
        raw_sha256_hex: hex_encode(&raw_digest),
        raw_mtime_secs,
        gzip_sha256_b64: base64_encode(&gzip_digest),
        gzip_sha256_hex: hex_encode(&gzip_digest),
        gzip,
    })
}

/// How one segment ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentOutcome {
    /// Uploaded to its canonical key, verified, marked.
    Uploaded { key: String },
    /// The canonical key held other bytes; uploaded to the sidecar key.
    UploadedSidecar { key: String },
    /// An identical object was already there; marked.
    Reused { key: String },
    /// No marker written.
    Failed { reason: String },
}

/// Puts `body` at `key` and verifies it with HeadObject. `Err` on any
/// mismatch: the caller writes no marker.
async fn put_and_verify<S: ColdObjectStore>(
    store: &S,
    key: &str,
    prepared: &PreparedSegment,
) -> Result<()> {
    let gzip_len = prepared.gzip.len() as u64;
    let metadata = [
        ("raw-sha256", prepared.raw_sha256_hex.clone()),
        ("raw-len", prepared.raw_len.to_string()),
    ];
    store
        .put_object_if_absent(
            key,
            prepared.gzip.clone(), // APPROVED: cold path, one copy per segment upload
            &prepared.gzip_sha256_b64,
            &metadata,
        )
        .await?;
    let head = store
        .head_object(key)
        .await
        .context("HeadObject after the upload")?;
    match classify_existing(head.as_ref(), gzip_len, &prepared.gzip_sha256_b64) {
        ExistingObject::Identical => Ok(()),
        ExistingObject::Absent => anyhow::bail!("object not found after the upload"),
        ExistingObject::Different => anyhow::bail!(
            "uploaded object does not verify: stored length {:?} / sha256 {:?}, expected {gzip_len} / {}",
            head.as_ref().map(|m| m.len),
            head.as_ref().and_then(|m| m.checksum_sha256_b64.as_deref()),
            prepared.gzip_sha256_b64
        ),
    }
}

/// Uploads one WAL segment to `raw-frames/<IST date>/<name>.gz` and writes
/// its marker, recording the segment's first frame sequence. Never panics;
/// every failure is a `Failed` outcome with no marker.
// TEST-EXEMPT: driven through run_pass in test_marker_matches_after_a_successful_upload, test_regression_size_mismatch_after_upload_writes_no_marker and the reuse/sidecar tests
pub async fn upload_segment<S: ColdObjectStore>(
    store: &S,
    markers_dir: &Path,
    segment: &Path,
) -> SegmentOutcome {
    let Some(name) = segment.file_name().and_then(|n| n.to_str()) else {
        return SegmentOutcome::Failed {
            reason: "segment path has no file name".to_string(),
        };
    };
    let path = segment.to_path_buf();
    let probed = tokio::task::spawn_blocking(move || {
        let mtime = std::fs::metadata(&path).map_or(0, |m| mtime_secs_of(&m));
        (
            mtime,
            crate::ws_frame_spill::first_frame_seq_in_segment(&path),
        )
    })
    .await;
    let (mtime, first_seq) = match probed {
        Ok(v) => v,
        Err(err) => {
            return SegmentOutcome::Failed {
                reason: format!("probe task failed: {err}"),
            };
        }
    };
    let key = segment_key(name, mtime);
    // `0` is the probe's "unknown" (v1 record, torn or unreadable first record).
    let first_frame_seq = (first_seq != 0).then_some(first_seq);
    upload_file_with(store, markers_dir, segment, &key, first_frame_seq).await
}

/// Uploads any one file to `key` (its canonical key; a conflicting object
/// there sends the bytes to the content-addressed sidecar instead), verifies
/// it, and writes its marker into `markers_dir`. Used for the spill and
/// quarantine sets; [`upload_segment`] is the WAL form. Never panics; every
/// failure is a `Failed` outcome with no marker.
pub async fn upload_file<S: ColdObjectStore>(
    store: &S,
    markers_dir: &Path,
    path: &Path,
    key: &str,
) -> SegmentOutcome {
    upload_file_with(store, markers_dir, path, key, None).await
}

/// [`upload_file`] with the marker's first frame sequence.
async fn upload_file_with<S: ColdObjectStore>(
    store: &S,
    markers_dir: &Path,
    segment: &Path,
    key: &str,
    first_frame_seq: Option<u64>,
) -> SegmentOutcome {
    let Some(name) = segment
        .file_name()
        .and_then(|n| n.to_str())
        .map(ToString::to_string)
    else {
        return SegmentOutcome::Failed {
            reason: "file path has no file name".to_string(),
        };
    };
    let path = segment.to_path_buf();
    let prepared = match tokio::task::spawn_blocking(move || prepare_segment(&path)).await {
        Ok(Ok(p)) => p,
        Ok(Err(err)) => {
            return SegmentOutcome::Failed {
                reason: format!("{err:#}"),
            };
        }
        Err(err) => {
            return SegmentOutcome::Failed {
                reason: format!("prepare task failed: {err}"),
            };
        }
    };
    let gzip_len = prepared.gzip.len() as u64;
    let canonical = key.to_string();

    let head = match store.head_object(&canonical).await {
        Ok(h) => h,
        Err(err) => {
            return SegmentOutcome::Failed {
                reason: format!("HeadObject {canonical}: {err:#}"),
            };
        }
    };
    let outcome = match classify_existing(head.as_ref(), gzip_len, &prepared.gzip_sha256_b64) {
        ExistingObject::Identical => SegmentOutcome::Reused { key: canonical },
        ExistingObject::Absent => match put_and_verify(store, &canonical, &prepared).await {
            Ok(()) => SegmentOutcome::Uploaded { key: canonical },
            Err(err) => {
                return SegmentOutcome::Failed {
                    reason: format!("{canonical}: {err:#}"),
                };
            }
        },
        ExistingObject::Different => {
            // Never overwrite: the bytes go to a content-addressed key.
            let side = sidecar_key(&canonical, &prepared.gzip_sha256_hex);
            let side_head = match store.head_object(&side).await {
                Ok(h) => h,
                Err(err) => {
                    return SegmentOutcome::Failed {
                        reason: format!("HeadObject {side}: {err:#}"),
                    };
                }
            };
            match classify_existing(side_head.as_ref(), gzip_len, &prepared.gzip_sha256_b64) {
                ExistingObject::Identical => SegmentOutcome::Reused { key: side },
                ExistingObject::Absent => match put_and_verify(store, &side, &prepared).await {
                    Ok(()) => SegmentOutcome::UploadedSidecar { key: side },
                    Err(err) => {
                        return SegmentOutcome::Failed {
                            reason: format!("{side}: {err:#}"),
                        };
                    }
                },
                ExistingObject::Different => {
                    return SegmentOutcome::Failed {
                        reason: format!(
                            "{canonical} and its content-addressed sidecar {side} both hold \
                             other bytes; neither overwritten"
                        ),
                    };
                }
            }
        }
    };
    let key = match &outcome {
        SegmentOutcome::Uploaded { key }
        | SegmentOutcome::UploadedSidecar { key }
        | SegmentOutcome::Reused { key } => key.clone(),
        SegmentOutcome::Failed { .. } => return outcome,
    };
    let marker = UploadMarker {
        version: MARKER_VERSION,
        bucket: store.bucket_name().to_string(),
        key,
        gzip_sha256: prepared.gzip_sha256_hex,
        gzip_len,
        raw_sha256: prepared.raw_sha256_hex,
        raw_len: prepared.raw_len,
        raw_mtime_secs: prepared.raw_mtime_secs,
        first_frame_seq,
    };
    let dir = markers_dir.to_path_buf();
    let marker_path = markers_dir.join(&name);
    let held = marker.clone();
    match tokio::task::spawn_blocking(move || write_marker(&dir, &name, &marker)).await {
        Ok(Ok(())) => outcome,
        Ok(Err(err)) => {
            // S2: the copy IS verified in S3; only the record of it failed
            // (most often a full disk). Hold it in memory so the prune can
            // still free this file, and retry the marker on a later pass.
            metrics::counter!(MARKER_WRITE_FAILED_COUNTER).increment(1);
            let remembered = remember_unmarked(marker_path, held);
            error!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                source = "raw_frame_upload_marker",
                remembered,
                error = %format!("{err:#}"),
                "a file's copy in S3 is verified but its marker could not be written; \
                 the verified copy is held in memory so the cleanup can still free \
                 the disk, and the marker is retried on the next pass"
            );
            SegmentOutcome::Failed {
                reason: format!(
                    "upload verified but the marker write failed (held in memory: \
                     {remembered}): {err:#}"
                ),
            }
        }
        Err(err) => SegmentOutcome::Failed {
            reason: format!("marker task failed: {err}"),
        },
    }
}

/// A directory a pass scans, and where the markers of its files live.
#[derive(Debug, Clone)]
struct ScanDir {
    dir: PathBuf,
    markers: PathBuf,
}

/// How a pass decides a file's existing marker still covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerMatch {
    /// Name and length: WAL segments, whose names are unique nanos and which
    /// are never rewritten once sealed (the WAL prune asks the same).
    Length,
    /// Name, length and mtime: spill and quarantine files, whose names repeat
    /// (the spill and quarantine prunes ask the same).
    LengthAndMtime,
}

/// A file without a valid marker.
#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    markers: PathBuf,
}

/// Files still needing an upload, in file-name order: every regular file in
/// `dirs` with `extension` (any, when `None`) that `is_open` does not claim,
/// that has been quiet for [`RAW_UPLOAD_MIN_QUIET_SECS`], and whose marker is
/// missing or does not match. Cold path: one directory walk per pass.
fn pending_files(
    dirs: &[ScanDir],
    extension: Option<&str>,
    rule: MarkerMatch,
    now: SystemTime,
    is_open: &dyn Fn(&Path) -> bool,
) -> Vec<Candidate> {
    // O(1) EXEMPT: begin — cold upload pass, one entry per file on disk
    let mut out: Vec<(std::ffi::OsString, Candidate)> = Vec::with_capacity(256);
    let quiet = Duration::from_secs(RAW_UPLOAD_MIN_QUIET_SECS);
    for scan in dirs {
        let Ok(entries) = std::fs::read_dir(&scan.dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if extension.is_some_and(|ext| path.extension().and_then(|s| s.to_str()) != Some(ext))
                || is_open(&path)
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let settled = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= quiet);
            let marked = match rule {
                MarkerMatch::Length => marker_matches(&scan.markers, &path, meta.len()),
                MarkerMatch::LengthAndMtime => {
                    marker_matches_file(&scan.markers, &path, meta.len(), mtime_secs_of(&meta))
                }
            };
            if !settled || marked {
                continue;
            }
            out.push((
                entry.file_name(),
                Candidate {
                    path,
                    markers: scan.markers.clone(),
                },
            ));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0)); // name order == capture order for WAL and day files
    // O(1) EXEMPT: end
    out.into_iter().map(|(_, c)| c).collect()
}

/// The WAL root, `replaying/` and `archive/`, all marked in `<wal>/uploaded`.
fn wal_scan_dirs(wal_dir: &Path) -> Vec<ScanDir> {
    let markers = markers_dir(wal_dir);
    [
        wal_dir.to_path_buf(),
        wal_dir.join("replaying"),
        wal_dir.join("archive"),
    ]
    .into_iter()
    .map(|dir| ScanDir {
        dir,
        markers: markers.clone(),
    })
    .collect()
}

/// Totals of one pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadPassSummary {
    pub uploaded: usize,
    pub uploaded_sidecar: usize,
    pub reused: usize,
    pub failed: usize,
    /// Files still without a valid marker when the pass ended.
    pub backlog_after: usize,
}

impl UploadPassSummary {
    /// Files newly marked this pass.
    #[must_use]
    pub const fn marked(&self) -> usize {
        self.uploaded + self.uploaded_sidecar + self.reused
    }

    /// Field-wise sum, for a caller that runs several sets.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            uploaded: self.uploaded + other.uploaded,
            uploaded_sidecar: self.uploaded_sidecar + other.uploaded_sidecar,
            reused: self.reused + other.reused,
            failed: self.failed + other.failed,
            backlog_after: self.backlog_after + other.backlog_after,
        }
    }
}

/// One set of non-WAL files the cold bucket keeps a verified copy of before
/// their prune may delete them (operator Quotes 27 + 28).
#[derive(Debug, Clone)]
pub struct ColdFileSet {
    /// Metric label and log name.
    pub label: &'static str,
    /// S3 key prefix.
    pub prefix: &'static str,
    /// Directories scanned; each keeps its markers in `<dir>/uploaded`.
    pub dirs: Vec<PathBuf>,
    /// File extension the set covers; `None` covers every regular file.
    pub extension: Option<&'static str>,
}

impl ColdFileSet {
    /// Sealed-candle spill files: the spill root, `replaying/` and
    /// `archive/`, `*.bin` — exactly what `seal_spill::prune_spill_files`
    /// may delete.
    #[must_use]
    pub fn seal_spill(spill_dir: &Path) -> Self {
        Self {
            label: "seal_spill",
            prefix: SEAL_SPILL_S3_PREFIX,
            dirs: vec![
                spill_dir.to_path_buf(),
                spill_dir.join(crate::seal_writer_task::SEAL_REPLAYING_SUBDIR),
                spill_dir.join(crate::seal_writer_task::SEAL_ARCHIVE_SUBDIR),
            ],
            extension: Some("bin"),
        }
    }

    /// Quarantined tick spill files: every file in `<tick spill>/quarantine`,
    /// exactly what `tick_persistence::prune_quarantine` may delete.
    #[must_use]
    pub fn tick_quarantine(tick_spill_dir: &Path) -> Self {
        Self {
            label: "tick_quarantine",
            prefix: TICK_QUARANTINE_S3_PREFIX,
            dirs: vec![tick_spill_dir.join(crate::tick_spill_replay::QUARANTINE_DIR)],
            extension: None,
        }
    }

    /// Quarantined depth spill files: every file in
    /// `<depth spill>/quarantine`.
    #[must_use]
    pub fn depth_quarantine(depth_spill_dir: &Path) -> Self {
        Self {
            label: "depth_quarantine",
            prefix: DEPTH_QUARANTINE_S3_PREFIX,
            dirs: vec![depth_spill_dir.join(crate::tick_spill_replay::QUARANTINE_DIR)],
            extension: None,
        }
    }

    fn scan_dirs(&self) -> Vec<ScanDir> {
        self.dirs
            .iter()
            .map(|dir| ScanDir {
                dir: dir.clone(),
                markers: dir.join(UPLOADED_SUBDIR),
            })
            .collect()
    }
}

/// Which files a pass covers and how their keys are built.
#[derive(Clone, Copy)]
enum PassKind<'a> {
    /// WAL segments: `raw-frames/<IST date of the name's nanos>/...`.
    Wal,
    /// A [`ColdFileSet`]: `<prefix>/<IST date of the mtime>/...`.
    Files(&'a ColdFileSet),
}

/// Uploads one candidate the way its pass kind needs.
async fn upload_candidate<S: ColdObjectStore>(
    store: &S,
    kind: PassKind<'_>,
    cand: &Candidate,
) -> SegmentOutcome {
    match kind {
        PassKind::Wal => upload_segment(store, &cand.markers, &cand.path).await,
        PassKind::Files(set) => {
            let Some(name) = cand.path.file_name().and_then(|n| n.to_str()) else {
                return SegmentOutcome::Failed {
                    reason: "file path has no file name".to_string(),
                };
            };
            let mtime = crate::off_worker::off_worker(|| {
                std::fs::metadata(&cand.path).map_or(0, |m| mtime_secs_of(&m))
            });
            let key = cold_file_key(set.prefix, name, mtime);
            upload_file(store, &cand.markers, &cand.path, &key).await
        }
    }
}

/// One upload pass over `wal_dir`, at most `max_segments`, stopping early
/// when `should_continue` says so or after [`MAX_CONSECUTIVE_FAILURES`]
/// failures in a row. Publishes the counters and the backlog gauge, and logs
/// at most one coded line per pass.
pub async fn run_pass<S: ColdObjectStore>(
    store: &S,
    wal_dir: &Path,
    max_segments: usize,
    should_continue: &(dyn Fn() -> bool + Send + Sync),
) -> UploadPassSummary {
    run_pass_with(
        store,
        wal_dir,
        max_segments,
        should_continue,
        &crate::ws_frame_spill::is_open_segment,
        SystemTime::now(),
    )
    .await
}

/// [`run_pass`] with the open-segment predicate and the clock injected.
async fn run_pass_with<S: ColdObjectStore>(
    store: &S,
    wal_dir: &Path,
    max_segments: usize,
    should_continue: &(dyn Fn() -> bool + Send + Sync),
    is_open: &(dyn Fn(&Path) -> bool + Send + Sync),
    now: SystemTime,
) -> UploadPassSummary {
    retry_held_markers().await;
    // 2026-10-03 (O(1) sweep S3): the listing stats every file in three
    // directories and reads a marker per file, so it moves the worker aside.
    let pending = crate::off_worker::off_worker(|| {
        pending_files(
            &wal_scan_dirs(wal_dir),
            Some("wal"),
            MarkerMatch::Length,
            now,
            is_open,
        )
    });
    let (summary, first_failure) = upload_candidates(
        store,
        PassKind::Wal,
        &pending,
        max_segments,
        should_continue,
    )
    .await;
    metrics::counter!(RAW_UPLOAD_OK_COUNTER).increment(summary.marked() as u64);
    metrics::counter!(RAW_UPLOAD_FAILED_COUNTER).increment(summary.failed as u64);
    // APPROVED: cast — a segment count, far below f64 precision loss.
    #[allow(clippy::cast_precision_loss)] // APPROVED: segment count
    metrics::gauge!(RAW_UPLOAD_BACKLOG_GAUGE).set(summary.backlog_after as f64);
    if let Some(first) = first_failure {
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            source = "raw_frame_upload",
            failed = summary.failed,
            marked = summary.marked(),
            backlog = summary.backlog_after,
            bucket = %store.bucket_name(),
            first_failure = %first,
            "raw WAL segments could not be copied to S3 and verified; they stay on disk \
             and the WAL cleanup will not delete them until a copy is verified. Retried \
             on the next pass."
        );
    } else if summary.marked() > 0 {
        info!(
            uploaded = summary.uploaded,
            uploaded_sidecar = summary.uploaded_sidecar,
            reused = summary.reused,
            backlog = summary.backlog_after,
            bucket = %store.bucket_name(),
            "raw WAL segments copied to S3 and verified"
        );
    }
    summary
}

/// One upload pass over a [`ColdFileSet`]: the same rules as [`run_pass`]
/// (quiet window, at most `max_files`, stop on `should_continue` or after
/// [`MAX_CONSECUTIVE_FAILURES`] in a row), its own counters labelled `set`,
/// and at most one coded line per pass.
pub async fn run_file_pass<S: ColdObjectStore>(
    store: &S,
    set: &ColdFileSet,
    max_files: usize,
    should_continue: &(dyn Fn() -> bool + Send + Sync),
    is_open: &(dyn Fn(&Path) -> bool + Send + Sync),
) -> UploadPassSummary {
    run_file_pass_with(
        store,
        set,
        max_files,
        should_continue,
        is_open,
        SystemTime::now(),
    )
    .await
}

/// [`run_file_pass`] with the clock injected.
async fn run_file_pass_with<S: ColdObjectStore>(
    store: &S,
    set: &ColdFileSet,
    max_files: usize,
    should_continue: &(dyn Fn() -> bool + Send + Sync),
    is_open: &(dyn Fn(&Path) -> bool + Send + Sync),
    now: SystemTime,
) -> UploadPassSummary {
    retry_held_markers().await;
    // Same as the WAL pass: the listing blocks on the disk.
    let pending = crate::off_worker::off_worker(|| {
        pending_files(
            &set.scan_dirs(),
            set.extension,
            MarkerMatch::LengthAndMtime,
            now,
            is_open,
        )
    });
    let (summary, first_failure) = upload_candidates(
        store,
        PassKind::Files(set),
        &pending,
        max_files,
        should_continue,
    )
    .await;
    metrics::counter!(COLD_FILE_UPLOAD_OK_COUNTER, "set" => set.label)
        .increment(summary.marked() as u64);
    metrics::counter!(COLD_FILE_UPLOAD_FAILED_COUNTER, "set" => set.label)
        .increment(summary.failed as u64);
    // APPROVED: cast — a file count, far below f64 precision loss.
    #[allow(clippy::cast_precision_loss)] // APPROVED: file count
    metrics::gauge!(COLD_FILE_UPLOAD_BACKLOG_GAUGE, "set" => set.label)
        .set(summary.backlog_after as f64);
    if let Some(first) = first_failure {
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            source = "cold_file_upload",
            set = set.label,
            failed = summary.failed,
            marked = summary.marked(),
            backlog = summary.backlog_after,
            bucket = %store.bucket_name(),
            first_failure = %first,
            "spill or quarantine files could not be copied to S3 and verified; they stay \
             on disk and their cleanup will not delete them until a copy is verified. \
             Retried on the next pass."
        );
    } else if summary.marked() > 0 {
        info!(
            set = set.label,
            uploaded = summary.uploaded,
            uploaded_sidecar = summary.uploaded_sidecar,
            reused = summary.reused,
            backlog = summary.backlog_after,
            bucket = %store.bucket_name(),
            "spill or quarantine files copied to S3 and verified"
        );
    }
    summary
}

/// The upload loop shared by both pass kinds. Returns the totals and the
/// first failure, for the caller's one coded line.
async fn upload_candidates<S: ColdObjectStore>(
    store: &S,
    kind: PassKind<'_>,
    pending: &[Candidate],
    max_files: usize,
    should_continue: &(dyn Fn() -> bool + Send + Sync),
) -> (UploadPassSummary, Option<String>) {
    let mut summary = UploadPassSummary::default();
    let mut consecutive_failures = 0usize;
    let mut first_failure: Option<String> = None;
    for cand in pending.iter().take(max_files) {
        if !should_continue() || consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            break;
        }
        match upload_candidate(store, kind, cand).await {
            SegmentOutcome::Uploaded { key } => {
                summary.uploaded += 1;
                consecutive_failures = 0;
                debug!(file = %cand.path.display(), key = %key, "file uploaded to the cold bucket and verified");
            }
            SegmentOutcome::UploadedSidecar { key } => {
                summary.uploaded_sidecar += 1;
                consecutive_failures = 0;
                info!(
                    file = %cand.path.display(),
                    key = %key,
                    "canonical key held other bytes; uploaded to a content-addressed key \
                     instead, nothing overwritten"
                );
            }
            SegmentOutcome::Reused { key } => {
                summary.reused += 1;
                consecutive_failures = 0;
                debug!(file = %cand.path.display(), key = %key, "file already in the cold bucket, verified; marked");
            }
            SegmentOutcome::Failed { reason } => {
                summary.failed += 1;
                consecutive_failures += 1;
                debug!(file = %cand.path.display(), reason = %reason, "cold-bucket upload failed");
                if first_failure.is_none() {
                    first_failure = Some(format!("{}: {reason}", cand.path.display()));
                }
            }
        }
    }
    summary.backlog_after = pending.len().saturating_sub(summary.marked());
    (summary, first_failure)
}

/// Uploads the sealed-candle spill files and the tick and depth quarantine
/// files, in that order, after the WAL pass (plan item 45e-1, the two
/// remaining ungated prunes). `today_spill_file` is the seal-spill day file
/// the live writer may hold open — never uploaded, as its prune never
/// deletes it. Each set gets at most `max_files`.
pub async fn run_spill_and_quarantine_passes<S: ColdObjectStore>(
    store: &S,
    spill_dir: &Path,
    tick_spill_dir: &Path,
    depth_spill_dir: &Path,
    today_spill_file: &str,
    max_files: usize,
    should_continue: &(dyn Fn() -> bool + Send + Sync),
) -> UploadPassSummary {
    let live = spill_dir.join(today_spill_file);
    let spill_open = move |p: &Path| p == live.as_path();
    let never_open = |_: &Path| false;
    let spill = run_file_pass(
        store,
        &ColdFileSet::seal_spill(spill_dir),
        max_files,
        should_continue,
        &spill_open,
    )
    .await;
    let ticks = run_file_pass(
        store,
        &ColdFileSet::tick_quarantine(tick_spill_dir),
        max_files,
        should_continue,
        &never_open,
    )
    .await;
    let depth = run_file_pass(
        store,
        &ColdFileSet::depth_quarantine(depth_spill_dir),
        max_files,
        should_continue,
        &never_open,
    )
    .await;
    spill.plus(ticks).plus(depth)
}

/// Test hook for the WAL prune's tests: records a marker for `segment`
/// claiming `raw_len` bytes. Kept below every production fn: the loss-counter
/// guard treats the file's first `#[cfg(test)]` as the end of production code.
#[cfg(test)]
// TEST-EXEMPT: test-only helper for the WAL prune tests
pub(crate) fn write_marker_for_test(markers_dir: &Path, segment: &Path, raw_len: u64) {
    let name = segment
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed"); // APPROVED: test-only
    let marker = UploadMarker {
        version: MARKER_VERSION,
        bucket: "tv-test-cold".to_string(),
        key: segment_key(name, 0),
        gzip_sha256: String::new(),
        gzip_len: 0,
        raw_sha256: String::new(),
        raw_len,
        raw_mtime_secs: 0,
        first_frame_seq: None,
    };
    write_marker(markers_dir, name, &marker).expect("write test marker"); // APPROVED: test-only
}

#[cfg(test)]
// TEST-EXEMPT: test-only helper for the spill and quarantine prune tests
/// Records a marker in `<file's dir>/uploaded/` matching `path`'s current
/// length and mtime, as a verified upload would.
pub(crate) fn write_file_marker_for_test(path: &Path) {
    let meta = std::fs::metadata(path).expect("stat"); // APPROVED: test-only
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed"); // APPROVED: test-only
    let marker = UploadMarker {
        version: MARKER_VERSION,
        bucket: "tv-test-cold".to_string(),
        key: cold_file_key("test", name, mtime_secs_of(&meta)),
        gzip_sha256: String::new(),
        gzip_len: 0,
        raw_sha256: String::new(),
        raw_len: meta.len(),
        raw_mtime_secs: mtime_secs_of(&meta),
        first_frame_seq: None,
    };
    let markers = sibling_markers_dir(path).expect("parent"); // APPROVED: test-only
    write_marker(&markers, name, &marker).expect("write test marker"); // APPROVED: test-only
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// 2026-10-03 (O(1) sweep S3): every listing of the upload directories
    /// stats one file per entry, so each production call moves the worker
    /// aside. A bare call would hold a tokio worker for a whole directory
    /// walk on a slow disk.
    #[test]
    fn every_listing_runs_off_the_worker() {
        let src = include_str!("raw_frame_upload.rs");
        let production = &src[..src.find("#[cfg(test)]").expect("test modules exist")];
        let bare_calls = production.matches("pending_files(").count();
        let wrapped = production
            .matches("off_worker(|| {\n        pending_files(")
            .count();
        // One definition, two wrapped calls.
        assert_eq!(bare_calls, 3, "pending_files: one definition and two calls");
        assert_eq!(
            wrapped, 2,
            "both pending_files calls must run inside off_worker"
        );
    }

    /// In-memory stand-in for the bucket. `head_len_skew` corrupts the
    /// length HeadObject reports for objects this store wrote.
    #[derive(Default)]
    struct FakeStore {
        objects: Mutex<HashMap<String, S3ObjectMeta>>,
        puts: Mutex<Vec<String>>,
        head_len_skew: u64,
        fail_heads: bool,
    }

    impl FakeStore {
        fn put_count(&self) -> usize {
            self.puts.lock().expect("lock").len() // APPROVED: test-only
        }
        fn preload(&self, key: &str, meta: S3ObjectMeta) {
            self.objects
                .lock()
                .expect("lock") // APPROVED: test-only
                .insert(key.to_string(), meta);
        }
    }

    impl ColdObjectStore for FakeStore {
        fn bucket_name(&self) -> &str {
            "tv-test-cold"
        }

        async fn head_object(&self, key: &str) -> Result<Option<S3ObjectMeta>> {
            if self.fail_heads {
                anyhow::bail!("network down");
            }
            Ok(self
                .objects
                .lock()
                .expect("lock") // APPROVED: test-only
                .get(key)
                .cloned())
        }

        async fn put_object_if_absent(
            &self,
            key: &str,
            body: Vec<u8>,
            sha256_b64: &str,
            _metadata: &[(&'static str, String)],
        ) -> Result<()> {
            let mut objects = self.objects.lock().expect("lock"); // APPROVED: test-only
            if objects.contains_key(key) {
                anyhow::bail!("412 precondition failed");
            }
            objects.insert(
                key.to_string(),
                S3ObjectMeta {
                    len: body.len() as u64 + self.head_len_skew,
                    checksum_sha256_b64: Some(sha256_b64.to_string()),
                },
            );
            self.puts.lock().expect("lock").push(key.to_string()); // APPROVED: test-only
            Ok(())
        }
    }

    const SEG: &str = "ws-frames-01790000000000000000.wal";

    fn temp_wal_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-raw-upload-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("mkdir"); // APPROVED: test-only
        dir
    }

    /// Writes a segment with an mtime well past the quiet window.
    fn plant(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write"); // APPROVED: test-only
        let f = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open"); // APPROVED: test-only
        f.set_modified(SystemTime::now() - Duration::from_secs(3_600))
            .expect("set mtime"); // APPROVED: test-only
        path
    }

    fn prepared_identity(body: &[u8]) -> (u64, String) {
        let dir = temp_wal_dir("ident");
        let p = plant(&dir, SEG, body);
        let prepared = prepare_segment(&p).expect("prepare"); // APPROVED: test-only
        (prepared.gzip.len() as u64, prepared.gzip_sha256_b64)
    }

    fn never_open(_: &Path) -> bool {
        false
    }

    fn always() -> bool {
        true
    }

    #[test]
    fn segment_key_uses_the_ist_date_of_the_segment_nanos() {
        // 1_790_000_000 s = 2026-09-21T13:33:20Z = 19:03:20 IST, same day.
        assert_eq!(
            segment_key(SEG, 0),
            "raw-frames/2026-09-21/ws-frames-01790000000000000000.wal.gz"
        );
        // 18:40 UTC is 00:10 IST the NEXT day.
        let name = format!("ws-frames-{:020}.wal", 1_790_016_000u128 * 1_000_000_000);
        assert!(segment_key(&name, 0).starts_with("raw-frames/2026-09-22/"));
        // No nanos in the name: the mtime decides.
        assert_eq!(
            segment_key("odd.wal", 1_790_000_000),
            "raw-frames/2026-09-21/odd.wal.gz"
        );
        assert_eq!(
            sidecar_key("raw-frames/2026-09-21/a.wal.gz", "ab12"),
            "raw-frames/2026-09-21/a.wal.sha-ab12.gz"
        );
    }

    #[test]
    fn test_upload_window_open_is_false_from_0900_to_1540_ist() {
        // IST = UTC + 5:30. 2026-09-21 00:00 IST = 2026-09-20T18:30:00Z.
        let midnight_ist: i64 = 1_789_929_000;
        let at = |h: i64, m: i64| midnight_ist + h * 3_600 + m * 60;
        assert!(upload_window_open(at(8, 59)));
        assert!(!upload_window_open(at(9, 0)));
        assert!(!upload_window_open(at(12, 0)));
        assert!(!upload_window_open(at(15, 39)));
        assert!(upload_window_open(at(15, 40)));
        assert!(upload_window_open(at(23, 0)));
    }

    #[test]
    fn prepare_segment_round_trips_and_hashes_the_raw_bytes() {
        use sha2::Digest as _;
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let dir = temp_wal_dir("prep");
        let p = plant(&dir, SEG, &body);
        let prepared = prepare_segment(&p).expect("prepare"); // APPROVED: test-only
        assert_eq!(prepared.raw_len, body.len() as u64);
        let digest: [u8; 32] = sha2::Sha256::digest(&body).into();
        assert_eq!(prepared.raw_sha256_hex, hex_encode(&digest));
        let mut back = Vec::new();
        GzDecoder::new(prepared.gzip.as_slice())
            .read_to_end(&mut back)
            .expect("gunzip"); // APPROVED: test-only
        assert_eq!(back, body);
    }

    #[tokio::test]
    async fn test_marker_matches_after_a_successful_upload() {
        let dir = temp_wal_dir("ok");
        let seg = plant(&dir, SEG, b"frames frames frames");
        let store = FakeStore::default();
        let s = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!((s.uploaded, s.failed, s.backlog_after), (1, 0, 0));
        let markers = markers_dir(&dir);
        assert!(marker_matches(&markers, &seg, 20));
        let m = read_marker(&markers, SEG).expect("marker"); // APPROVED: test-only
        assert_eq!(m.key, segment_key(SEG, 0));
        assert_eq!(m.bucket, "tv-test-cold");
        assert!(
            !marker_matches(&markers, &seg, 21),
            "a marker for another length must not satisfy the prune"
        );
        // A second pass finds nothing to do and uploads nothing again.
        let again = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!(again.marked(), 0);
        assert_eq!(store.put_count(), 1);
    }

    #[tokio::test]
    async fn test_regression_size_mismatch_after_upload_writes_no_marker() {
        let dir = temp_wal_dir("skew");
        let seg = plant(&dir, SEG, b"frames frames frames");
        let store = FakeStore {
            head_len_skew: 1,
            ..FakeStore::default()
        };
        let s = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!((s.marked(), s.failed, s.backlog_after), (0, 1, 1));
        assert!(!marker_matches(&markers_dir(&dir), &seg, 20));
        assert!(!markers_dir(&dir).join(SEG).exists());
    }

    #[tokio::test]
    async fn an_identical_existing_object_is_reused_without_a_put() {
        let dir = temp_wal_dir("reuse");
        let body = b"the same bytes as before the crash";
        let seg = plant(&dir, SEG, body);
        let (len, sha) = prepared_identity(body);
        let store = FakeStore::default();
        store.preload(
            &segment_key(SEG, 0),
            S3ObjectMeta {
                len,
                checksum_sha256_b64: Some(sha),
            },
        );
        let s = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!((s.reused, s.uploaded, s.failed), (1, 0, 0));
        assert_eq!(store.put_count(), 0, "nothing re-uploaded");
        assert!(marker_matches(&markers_dir(&dir), &seg, body.len() as u64));
    }

    #[tokio::test]
    async fn a_conflicting_object_sends_the_bytes_to_the_sidecar_key() {
        let dir = temp_wal_dir("side");
        let body = b"these bytes differ from the object in the bucket";
        let seg = plant(&dir, SEG, body);
        let store = FakeStore::default();
        let canonical = segment_key(SEG, 0);
        store.preload(
            &canonical,
            S3ObjectMeta {
                len: 7,
                checksum_sha256_b64: Some("other".to_string()),
            },
        );
        let s = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!((s.uploaded_sidecar, s.failed), (1, 0));
        let m = read_marker(&markers_dir(&dir), SEG).expect("marker"); // APPROVED: test-only
        assert!(
            m.key
                .starts_with(canonical.strip_suffix(".gz").unwrap_or(""))
        );
        assert!(m.key.contains(".sha-"));
        assert_ne!(m.key, canonical);
        // The canonical object is untouched.
        let objects = store.objects.lock().expect("lock"); // APPROVED: test-only
        assert_eq!(objects.get(&canonical).map(|o| o.len), Some(7));
        drop(objects);
        assert!(marker_matches(&markers_dir(&dir), &seg, body.len() as u64));
    }

    #[tokio::test]
    async fn the_open_segment_and_a_fresh_segment_are_skipped() {
        let dir = temp_wal_dir("open");
        let open = plant(&dir, SEG, b"still being written");
        let fresh = dir.join("ws-frames-01790000000000000001.wal");
        std::fs::write(&fresh, b"just rotated").expect("write"); // APPROVED: test-only
        let store = FakeStore::default();
        let is_open = move |p: &Path| p == open.as_path();
        let s = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &is_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!(s.marked() + s.failed, 0);
        assert_eq!(store.put_count(), 0);
    }

    #[tokio::test]
    async fn replaying_and_archive_segments_are_uploaded_oldest_first_within_the_cap() {
        let dir = temp_wal_dir("dirs");
        std::fs::create_dir_all(dir.join("replaying")).expect("mkdir"); // APPROVED: test-only
        std::fs::create_dir_all(dir.join("archive")).expect("mkdir"); // APPROVED: test-only
        plant(
            &dir.join("archive"),
            "ws-frames-01790000000000000001.wal",
            b"a",
        );
        plant(
            &dir.join("replaying"),
            "ws-frames-01790000000000000002.wal",
            b"b",
        );
        plant(&dir, "ws-frames-01790000000000000003.wal", b"c");
        let store = FakeStore::default();
        let s = run_pass_with(&store, &dir, 2, &always, &never_open, SystemTime::now()).await;
        assert_eq!((s.uploaded, s.backlog_after), (2, 1));
        let puts = store.puts.lock().expect("lock").clone(); // APPROVED: test-only
        assert!(puts[0].ends_with("01.wal.gz") && puts[1].ends_with("02.wal.gz"));
    }

    #[tokio::test]
    async fn an_unreachable_store_stops_the_pass_and_writes_no_marker() {
        let dir = temp_wal_dir("down");
        for i in 0..5 {
            plant(&dir, &format!("ws-frames-0179000000000000000{i}.wal"), b"x");
        }
        let store = FakeStore {
            fail_heads: true,
            ..FakeStore::default()
        };
        let s = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!(s.failed, MAX_CONSECUTIVE_FAILURES);
        assert_eq!(s.backlog_after, 5);
        assert!(!markers_dir(&dir).exists());
    }

    #[test]
    fn remove_marker_deletes_only_that_marker() {
        let dir = temp_wal_dir("rm");
        let markers = markers_dir(&dir);
        let marker = UploadMarker {
            version: MARKER_VERSION,
            bucket: "b".into(),
            key: "k".into(),
            gzip_sha256: "g".into(),
            gzip_len: 1,
            raw_sha256: "r".into(),
            raw_len: 5,
            raw_mtime_secs: 0,
            first_frame_seq: None,
        };
        write_marker(&markers, "a.wal", &marker).expect("write"); // APPROVED: test-only
        write_marker(&markers, "b.wal", &marker).expect("write"); // APPROVED: test-only
        remove_marker(&markers, &dir.join("a.wal"));
        assert!(!markers.join("a.wal").exists());
        assert!(marker_matches(&markers, &dir.join("b.wal"), 5));
        assert!(!markers.join("a.wal.tmp").exists(), "no tmp left behind");
    }

    // ---- plan item 45e-1: spill and quarantine copies ----

    const DAY_FILE: &str = "seals_v4-2026-09-21.bin";

    #[test]
    fn test_cold_file_key_uses_the_prefix_and_the_ist_date_of_the_mtime() {
        // 1_790_000_000 s = 2026-09-21T13:33:20Z = 19:03:20 IST.
        assert_eq!(
            cold_file_key(SEAL_SPILL_S3_PREFIX, DAY_FILE, 1_790_000_000),
            "seal-spill/2026-09-21/seals_v4-2026-09-21.bin.gz"
        );
        // 18:40 UTC is 00:10 IST the next day.
        assert_eq!(
            cold_file_key(TICK_QUARANTINE_S3_PREFIX, "ticks-1.ndjson", 1_790_016_000),
            "tick-quarantine/2026-09-22/ticks-1.ndjson.gz"
        );
        assert!(cold_file_key(DEPTH_QUARANTINE_S3_PREFIX, "d", 0).starts_with("depth-quarantine/"));
    }

    #[test]
    fn test_marker_matches_file_needs_the_length_and_the_mtime() {
        let dir = temp_wal_dir("mf");
        let file = plant(&dir, DAY_FILE, b"seal records");
        let meta = std::fs::metadata(&file).expect("stat"); // APPROVED: test-only
        let markers = sibling_markers_dir(&file).expect("parent"); // APPROVED: test-only
        assert!(!marker_matches_file(
            &markers,
            &file,
            meta.len(),
            mtime_secs_of(&meta)
        ));
        write_file_marker_for_test(&file);
        let mtime = mtime_secs_of(&meta);
        assert!(marker_matches_file(&markers, &file, meta.len(), mtime));
        assert!(!marker_matches_file(&markers, &file, meta.len() + 1, mtime));
        assert!(
            !marker_matches_file(&markers, &file, meta.len(), mtime + 1),
            "a recreated day file of the same length is a different file"
        );
    }

    #[test]
    fn test_write_file_marker_for_test_records_the_files_length_and_mtime() {
        let dir = temp_wal_dir("helper");
        let file = plant(&dir, DAY_FILE, b"twelve bytes");
        write_file_marker_for_test(&file);
        let m = read_marker(&dir.join(UPLOADED_SUBDIR), DAY_FILE).expect("marker"); // APPROVED: test-only
        let meta = std::fs::metadata(&file).expect("stat"); // APPROVED: test-only
        assert_eq!(m.raw_len, 12);
        assert_eq!(m.raw_mtime_secs, mtime_secs_of(&meta));
    }

    #[test]
    fn test_sibling_markers_dir_is_uploaded_beside_the_file() {
        assert_eq!(
            sibling_markers_dir(Path::new("/x/spill/archive/a.bin")),
            Some(PathBuf::from("/x/spill/archive/uploaded"))
        );
        assert_eq!(sibling_markers_dir(Path::new("/")), None);
    }

    #[test]
    fn test_copy_gate_from_config_allows_delete_copy_in_s3_and_forget() {
        assert_eq!(CopyGate::from_config(false), CopyGate::NotRequired);
        assert_eq!(CopyGate::from_config(true), CopyGate::Required);
        assert!(!CopyGate::NotRequired.copy_in_s3());
        assert!(CopyGate::Required.copy_in_s3());

        let dir = temp_wal_dir("gate");
        let file = plant(&dir, DAY_FILE, b"seal records");
        let meta = std::fs::metadata(&file).expect("stat"); // APPROVED: test-only
        assert!(CopyGate::NotRequired.allows_delete(&file, &meta));
        assert!(
            !CopyGate::Required.allows_delete(&file, &meta),
            "no marker, no delete"
        );
        write_file_marker_for_test(&file);
        assert!(CopyGate::Required.allows_delete(&file, &meta));

        // The file grows after the upload: the copy no longer covers it.
        std::fs::write(&file, b"seal records and one more").expect("write"); // APPROVED: test-only
        let grown = std::fs::metadata(&file).expect("stat"); // APPROVED: test-only
        assert!(!CopyGate::Required.allows_delete(&file, &grown));

        CopyGate::NotRequired.forget(&file);
        assert!(dir.join(UPLOADED_SUBDIR).join(DAY_FILE).exists());
        CopyGate::Required.forget(&file);
        assert!(!dir.join(UPLOADED_SUBDIR).join(DAY_FILE).exists());
    }

    #[test]
    fn test_upload_pass_summary_plus_adds_every_field() {
        let a = UploadPassSummary {
            uploaded: 1,
            uploaded_sidecar: 2,
            reused: 3,
            failed: 4,
            backlog_after: 5,
        };
        let b = UploadPassSummary {
            uploaded: 10,
            uploaded_sidecar: 20,
            reused: 30,
            failed: 40,
            backlog_after: 50,
        };
        let s = a.plus(b);
        assert_eq!(
            (
                s.uploaded,
                s.uploaded_sidecar,
                s.reused,
                s.failed,
                s.backlog_after
            ),
            (11, 22, 33, 44, 55)
        );
        assert_eq!(s.marked(), 66);
    }

    #[test]
    fn test_cold_file_set_seal_spill_tick_quarantine_depth_quarantine_dirs() {
        let spill = ColdFileSet::seal_spill(Path::new("/d/spill"));
        assert_eq!(spill.prefix, SEAL_SPILL_S3_PREFIX);
        assert_eq!(spill.extension, Some("bin"));
        assert_eq!(
            spill.dirs,
            vec![
                PathBuf::from("/d/spill"),
                PathBuf::from("/d/spill/replaying"),
                PathBuf::from("/d/spill/archive"),
            ]
        );
        let ticks = ColdFileSet::tick_quarantine(Path::new("/d/spill/ticks"));
        assert_eq!(ticks.prefix, TICK_QUARANTINE_S3_PREFIX);
        assert_eq!(ticks.dirs, vec![PathBuf::from("/d/spill/ticks/quarantine")]);
        assert_eq!(ticks.extension, None);
        let depth = ColdFileSet::depth_quarantine(Path::new("/d/spill/depth"));
        assert_eq!(depth.prefix, DEPTH_QUARANTINE_S3_PREFIX);
        assert_eq!(depth.dirs, vec![PathBuf::from("/d/spill/depth/quarantine")]);
        assert_ne!(ticks.label, depth.label);
    }

    #[tokio::test]
    async fn test_upload_file_puts_under_the_given_key_and_marks_beside_the_file() {
        let dir = temp_wal_dir("file");
        let file = plant(&dir, DAY_FILE, b"seal records");
        let store = FakeStore::default();
        let markers = dir.join(UPLOADED_SUBDIR);
        let key = "seal-spill/2026-09-21/seals_v4-2026-09-21.bin.gz";
        let out = upload_file(&store, &markers, &file, key).await;
        assert!(
            matches!(out, SegmentOutcome::Uploaded { ref key } if key.starts_with("seal-spill/"))
        );
        let m = read_marker(&markers, DAY_FILE).expect("marker"); // APPROVED: test-only
        assert_eq!(m.key, key);
        assert_eq!(m.first_frame_seq, None);
        let meta = std::fs::metadata(&file).expect("stat"); // APPROVED: test-only
        assert!(CopyGate::Required.allows_delete(&file, &meta));
    }

    #[tokio::test]
    async fn test_run_file_pass_uploads_every_spill_folder_and_a_second_pass_does_nothing() {
        let dir = temp_wal_dir("spillpass");
        std::fs::create_dir_all(dir.join("replaying")).expect("mkdir"); // APPROVED: test-only
        std::fs::create_dir_all(dir.join("archive")).expect("mkdir"); // APPROVED: test-only
        let top = plant(&dir, "seals_v4-2026-09-19.bin", b"a");
        let staged = plant(&dir.join("replaying"), "seals_v4-2026-09-19.bin.1", b"bb");
        let archived = plant(&dir.join("archive"), DAY_FILE, b"ccc");
        let foreign = plant(&dir, "operator-notes.txt", b"not ours");
        let set = ColdFileSet::seal_spill(&dir);
        let store = FakeStore::default();
        let never = |_: &Path| false;
        let s =
            run_file_pass_with(&store, &set, usize::MAX, &always, &never, SystemTime::now()).await;
        assert_eq!((s.uploaded, s.failed, s.backlog_after), (2, 0, 0));
        // `.bin.1` is not a `.bin` file: the spill prune never deletes it
        // either, so the pass leaves it alone.
        for f in [&top, &archived] {
            let meta = std::fs::metadata(f).expect("stat"); // APPROVED: test-only
            assert!(CopyGate::Required.allows_delete(f, &meta));
        }
        assert!(!dir.join("replaying").join(UPLOADED_SUBDIR).exists());
        assert!(staged.exists() && foreign.exists());
        let puts = store.puts.lock().expect("lock").clone(); // APPROVED: test-only
        assert!(puts.iter().all(|k| k.starts_with("seal-spill/")));
        let again = run_file_pass(&store, &set, usize::MAX, &always, &never).await;
        assert_eq!(again.marked() + again.failed, 0);
        assert_eq!(store.put_count(), 2);
    }

    #[tokio::test]
    async fn test_regression_file_pass_size_mismatch_writes_no_marker() {
        let dir = temp_wal_dir("fileskew");
        let file = plant(&dir, DAY_FILE, b"seal records");
        let store = FakeStore {
            head_len_skew: 1,
            ..FakeStore::default()
        };
        let set = ColdFileSet::seal_spill(&dir);
        let never = |_: &Path| false;
        let s =
            run_file_pass_with(&store, &set, usize::MAX, &always, &never, SystemTime::now()).await;
        assert_eq!((s.marked(), s.failed, s.backlog_after), (0, 1, 1));
        let meta = std::fs::metadata(&file).expect("stat"); // APPROVED: test-only
        assert!(!CopyGate::Required.allows_delete(&file, &meta));
        assert!(!dir.join(UPLOADED_SUBDIR).join(DAY_FILE).exists());
    }

    #[tokio::test]
    async fn test_run_spill_and_quarantine_passes_skips_today_and_covers_both_quarantines() {
        let spill = temp_wal_dir("all");
        let ticks = spill.join("ticks");
        let depth = spill.join("depth");
        std::fs::create_dir_all(ticks.join("quarantine")).expect("mkdir"); // APPROVED: test-only
        std::fs::create_dir_all(depth.join("quarantine")).expect("mkdir"); // APPROVED: test-only
        let today = plant(&spill, DAY_FILE, b"live writer file");
        let old = plant(&spill, "seals_v4-2026-09-18.bin", b"old");
        let tq = plant(&ticks.join("quarantine"), "ticks-a.ndjson", b"t");
        let dq = plant(&depth.join("quarantine"), "depth-a.ndjson", b"d");
        // A file still in the tick spill root (not quarantined) is not this
        // pass's business.
        let pending_tick = plant(&ticks, "ticks-b.ndjson", b"pending");
        let store = FakeStore::default();
        let s = run_spill_and_quarantine_passes(
            &store,
            &spill,
            &ticks,
            &depth,
            DAY_FILE,
            usize::MAX,
            &always,
        )
        .await;
        assert_eq!((s.uploaded, s.failed), (3, 0));
        for f in [&old, &tq, &dq] {
            let meta = std::fs::metadata(f).expect("stat"); // APPROVED: test-only
            assert!(
                CopyGate::Required.allows_delete(f, &meta),
                "{}",
                f.display()
            );
        }
        for f in [&today, &pending_tick] {
            let meta = std::fs::metadata(f).expect("stat"); // APPROVED: test-only
            assert!(
                !CopyGate::Required.allows_delete(f, &meta),
                "{}",
                f.display()
            );
        }
        let puts = store.puts.lock().expect("lock").clone(); // APPROVED: test-only
        assert!(puts.iter().any(|k| k.starts_with("tick-quarantine/")));
        assert!(puts.iter().any(|k| k.starts_with("depth-quarantine/")));
    }

    // S2 — a verified upload whose marker cannot be written still lets the
    // prune free the disk, and the marker is retried.

    #[tokio::test]
    async fn test_regression_s2_verified_upload_with_failed_marker_write_satisfies_the_prune() {
        let dir = temp_wal_dir("s2-enospc");
        let seg = plant(&dir, SEG, b"frames frames frames");
        let markers = markers_dir(&dir);
        // A regular file where the marker directory goes: every marker write
        // fails, the shape a full disk produces.
        std::fs::write(&markers, b"blocker").expect("blocker"); // APPROVED: test-only
        let store = FakeStore::default();
        let first = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!(first.failed, 1, "a missing marker is still reported");
        assert_eq!(store.put_count(), 1, "the copy is in S3");
        assert!(
            marker_matches(&markers, &seg, 20),
            "the verified upload satisfies the prune although no marker is on disk"
        );
        assert!(
            !marker_matches(&markers, &seg, 21),
            "a different length is never covered"
        );
        // The next pass does not upload it again.
        let second = run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        assert_eq!(second.backlog_after, 0);
        assert_eq!(store.put_count(), 1);

        // Space comes back: the next pass writes the marker and drops the
        // in-memory record.
        std::fs::remove_file(&markers).expect("unblock"); // APPROVED: test-only
        run_pass_with(
            &store,
            &dir,
            usize::MAX,
            &always,
            &never_open,
            SystemTime::now(),
        )
        .await;
        let on_disk = std::fs::read(markers.join(SEG)).expect("marker written"); // APPROVED: test-only
        let m: UploadMarker = serde_json::from_slice(&on_disk).expect("marker json"); // APPROVED: test-only
        assert_eq!(m.raw_len, 20);
        assert!(!unmarked_verified().contains_key(&markers.join(SEG)));
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_regression_s2_unverified_file_is_never_covered_and_a_delete_forgets_the_record() {
        let dir = temp_wal_dir("s2-forget");
        let markers = markers_dir(&dir);
        let seg = plant(&dir, SEG, b"0123456789");
        let other = plant(&dir, "ws-frames-01790000000000000001.wal", b"0123456789");
        assert!(remember_unmarked(
            markers.join(SEG),
            UploadMarker {
                version: MARKER_VERSION,
                bucket: "tv-test-cold".to_string(),
                key: segment_key(SEG, 0),
                gzip_sha256: String::new(),
                gzip_len: 0,
                raw_sha256: String::new(),
                raw_len: 10,
                raw_mtime_secs: 0,
                first_frame_seq: None,
            },
        ));
        assert!(marker_matches(&markers, &seg, 10));
        assert!(
            !marker_matches(&markers, &other, 10),
            "only a file this process verified is covered"
        );
        remove_marker(&markers, &seg);
        assert!(
            !marker_matches(&markers, &seg, 10),
            "a pruned file's record is dropped with it"
        );
        drop(std::fs::remove_dir_all(&dir));
    }
}
