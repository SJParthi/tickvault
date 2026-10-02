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

/// Marker directory under the WAL root.
pub const UPLOADED_SUBDIR: &str = "uploaded";

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
}

/// `<wal_dir>/uploaded`.
#[must_use]
pub fn markers_dir(wal_dir: &Path) -> PathBuf {
    wal_dir.join(UPLOADED_SUBDIR)
}

/// Reads the marker for `segment_name`, `None` when absent or unreadable.
fn read_marker(markers_dir: &Path, segment_name: &str) -> Option<UploadMarker> {
    let bytes = std::fs::read(markers_dir.join(segment_name)).ok()?;
    serde_json::from_slice::<UploadMarker>(&bytes).ok()
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

/// Removes `segment`'s marker after the prune deleted the segment.
/// Best effort: a leftover marker is harmless (see the module docs).
pub fn remove_marker(markers_dir: &Path, segment: &Path) {
    if let Some(name) = segment.file_name() {
        // O(1) EXEMPT: cold prune path, one unlink after a segment unlink
        drop(std::fs::remove_file(markers_dir.join(name)));
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

/// Test hook for the WAL prune's tests: records a marker for `segment`
/// claiming `raw_len` bytes.
#[cfg(test)]
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
    };
    write_marker(markers_dir, name, &marker).expect("write test marker"); // APPROVED: test-only
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
    format!("{RAW_FRAME_S3_PREFIX}/{}/{segment_name}.gz", ist_date(secs))
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
    let mut encoder = GzEncoder::new(
        Sha256Writer::new(Vec::with_capacity(GZIP_PRESIZE_BYTES)),
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

/// Uploads one segment and writes its marker. Never panics; every failure is
/// a `Failed` outcome with no marker.
pub async fn upload_segment<S: ColdObjectStore>(
    store: &S,
    markers_dir: &Path,
    segment: &Path,
) -> SegmentOutcome {
    let Some(name) = segment
        .file_name()
        .and_then(|n| n.to_str())
        .map(ToString::to_string)
    else {
        return SegmentOutcome::Failed {
            reason: "segment path has no file name".to_string(),
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
    let canonical = segment_key(&name, prepared.raw_mtime_secs);

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
    };
    let dir = markers_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || write_marker(&dir, &name, &marker)).await {
        Ok(Ok(())) => outcome,
        Ok(Err(err)) => SegmentOutcome::Failed {
            reason: format!("upload verified but the marker write failed: {err:#}"),
        },
        Err(err) => SegmentOutcome::Failed {
            reason: format!("marker task failed: {err}"),
        },
    }
}

/// A sealed segment without a valid marker.
#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
}

/// Sealed segments still needing an upload, oldest first: every `*.wal` in
/// the WAL root, `replaying/` and `archive/` that `is_open` does not claim,
/// that has been quiet for [`RAW_UPLOAD_MIN_QUIET_SECS`], and whose marker is
/// missing or records a different length. Cold path: one directory walk per
/// pass.
fn pending_segments(
    wal_dir: &Path,
    now: SystemTime,
    is_open: &dyn Fn(&Path) -> bool,
) -> Vec<Candidate> {
    let markers = markers_dir(wal_dir);
    // O(1) EXEMPT: cold upload pass, one entry per segment on disk
    let mut out: Vec<(std::ffi::OsString, Candidate)> = Vec::with_capacity(256);
    let quiet = Duration::from_secs(RAW_UPLOAD_MIN_QUIET_SECS);
    for dir in [
        wal_dir.to_path_buf(),
        wal_dir.join("replaying"),
        wal_dir.join("archive"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("wal") || is_open(&path) {
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
            if !settled || marker_matches(&markers, &path, meta.len()) {
                continue;
            }
            out.push((entry.file_name(), Candidate { path }));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0)); // O(1) EXEMPT: cold pass; name order == capture order
    out.into_iter().map(|(_, c)| c).collect()
}

/// Totals of one pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadPassSummary {
    pub uploaded: usize,
    pub uploaded_sidecar: usize,
    pub reused: usize,
    pub failed: usize,
    /// Segments still without a valid marker when the pass ended.
    pub backlog_after: usize,
}

impl UploadPassSummary {
    /// Segments newly marked this pass.
    #[must_use]
    pub const fn marked(&self) -> usize {
        self.uploaded + self.uploaded_sidecar + self.reused
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
    let wal_dir_owned = wal_dir.to_path_buf();
    let pending = pending_segments(&wal_dir_owned, now, is_open);
    let markers = markers_dir(wal_dir);
    let mut summary = UploadPassSummary::default();
    let mut consecutive_failures = 0usize;
    let mut first_failure: Option<String> = None;
    for cand in pending.iter().take(max_segments) {
        if !should_continue() || consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            break;
        }
        match upload_segment(store, &markers, &cand.path).await {
            SegmentOutcome::Uploaded { key } => {
                summary.uploaded += 1;
                consecutive_failures = 0;
                debug!(segment = %cand.path.display(), key = %key, "raw WAL segment uploaded and verified");
            }
            SegmentOutcome::UploadedSidecar { key } => {
                summary.uploaded_sidecar += 1;
                consecutive_failures = 0;
                info!(
                    segment = %cand.path.display(),
                    key = %key,
                    "raw WAL segment's canonical key held other bytes; uploaded to a \
                     content-addressed key instead, nothing overwritten"
                );
            }
            SegmentOutcome::Reused { key } => {
                summary.reused += 1;
                consecutive_failures = 0;
                debug!(segment = %cand.path.display(), key = %key, "raw WAL segment already in S3, verified; marked");
            }
            SegmentOutcome::Failed { reason } => {
                summary.failed += 1;
                consecutive_failures += 1;
                debug!(segment = %cand.path.display(), reason = %reason, "raw WAL segment upload failed");
                if first_failure.is_none() {
                    first_failure = Some(format!("{}: {reason}", cand.path.display()));
                }
            }
        }
    }
    summary.backlog_after = pending.len().saturating_sub(summary.marked());
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

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
    fn upload_window_is_closed_from_0900_to_1540_ist() {
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
    async fn success_writes_a_marker_the_prune_accepts() {
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
        };
        write_marker(&markers, "a.wal", &marker).expect("write"); // APPROVED: test-only
        write_marker(&markers, "b.wal", &marker).expect("write"); // APPROVED: test-only
        remove_marker(&markers, &dir.join("a.wal"));
        assert!(!markers.join("a.wal").exists());
        assert!(marker_matches(&markers, &dir.join("b.wal"), 5));
        assert!(!markers.join("a.wal.tmp").exists(), "no tmp left behind");
    }
}
