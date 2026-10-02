//! Raw capture-at-receipt WAL segments to S3 before any local delete (plan
//! item 45e, 2026-10-01).
//!
//! # Why this exists
//!
//! The WAL segment is the raw record of everything the sockets delivered. The
//! WAL prunes (age, byte ceiling, hard disk floor) delete segments on the box,
//! and until this module nothing else held a copy. Operator Quotes 27 + 28
//! (2026-09-29, `daily-universe-scope-expansion-2026-05-27.md`): "Raw
//! capture-at-receipt WAL segments … Uploaded to `s3://tv-prod-cold/raw-frames/`
//! (compressed, checksum-verified) BEFORE any local prune may delete them. A
//! segment with no verified S3 copy is never deleted by the age pass or the
//! byte pass."
//!
//! # Shape
//!
//! [`RawFrameUploader::run_forever`] runs on its own tokio task (never the
//! frame drain). Each pass lists the closed segments in the WAL root,
//! `replaying/` and `archive/`, oldest first, on a blocking thread, and for
//! each one without a valid marker:
//!
//! 1. gzips it (level 1) into `<wal_root>/uploaded/.tmp/`, hashing both the raw
//!    bytes and the gzip stream with SHA-256 as they pass (one read of the
//!    segment, on a blocking thread);
//! 2. re-reads the segment length and skips the segment if it changed or moved
//!    (a segment still being written is never uploaded);
//! 3. asks S3 whether the key already exists. The key carries the raw length
//!    (`<segment>.<raw bytes>.gz`), so a segment that grew after an upload gets
//!    a new key. An object counts as our copy when its length and SHA-256
//!    match, or when the raw SHA-256 stored in its metadata matches (the same
//!    raw bytes compressed by a different build). Any other object is a
//!    conflict and is never overwritten;
//! 4. otherwise PUTs with `If-None-Match: *`, `x-amz-checksum-sha256` (S3
//!    rejects a body that does not match the digest) and the raw SHA-256 and
//!    length as object metadata;
//! 5. reads the object back with HEAD and requires a match as in step 3;
//! 6. only then writes the marker `<wal_root>/uploaded/<segment>.uploaded`
//!    (temporary file, fsync, rename) on a blocking thread.
//!
//! A segment that is growing, in conflict, or failing locally is skipped and
//! the pass moves to the next one, so one bad segment never holds back the
//! copies that let the prunes free space. Only an S3 failure ends the pass
//! early. A conflict is logged once per process and then skipped quietly.
//!
//! The prunes in `ws_frame_spill` delete a segment only when
//! [`segment_has_verified_copy`] finds that marker and its recorded raw length
//! covers the segment's current length. Markers live in one directory keyed by
//! the segment's file name, so a segment moving between the WAL root,
//! `replaying/` and `archive/` keeps its marker.
//!
//! # Complexity
//!
//! Cold path, on its own task; every file system call runs on a blocking
//! thread. One pass is O(segments) directory entries, one `stat` and at most
//! one marker read (under 300 bytes) per segment, and O(segment bytes) of gzip
//! per uploaded segment. At most one segment is uploaded per pass, and passes
//! are spaced [`RAW_UPLOAD_SPACING_SECS`] apart while a backlog remains, so a
//! backlog never takes a core away from the live lane for long. The marker
//! check the prunes call is one read of a file under 300 bytes per candidate
//! segment, on the six-hourly prune task.
//!
//! # Honest limits
//!
//! - With no S3 copy nothing is deleted, so an S3 outage that outlasts the
//!   disk fills the volume. The prunes log the refusals; the disk alarms page.
//! - The gzip scratch file sits on the same volume as the WAL (about 0.46 of
//!   the segment), so a volume within one segment of full cannot make the copy
//!   that would free it.
//! - A box with no explicit environment (`TV_ENVIRONMENT` / `ENVIRONMENT`
//!   unset, i.e. a dev machine) builds no uploader, and the prunes then delete
//!   without a copy as before; the boot logs that as a coded error. Production
//!   sets `TV_ENVIRONMENT=prod` in the systemd unit.
//! - The instance role may write anywhere in the cold bucket; no-overwrite is
//!   enforced by `If-None-Match` from this client, not by IAM. The bucket's
//!   versioning (item 45f) keeps an overwritten object recoverable.
//! - The S3 checksum check is assumed from the S3 API contract; it is
//!   exercised here against a local stub, not against S3 itself.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use tickvault_common::error_code::ErrorCode;
use tracing::{debug, error, info, warn};

/// S3 key prefix for raw WAL segments in the cold bucket. The lifecycle rule
/// `raw-frames-deep-archive` in `deploy/aws/terraform/main.tf` (item 45f)
/// moves this prefix to Deep Archive after 30 days.
pub const RAW_FRAMES_S3_PREFIX: &str = "raw-frames";

/// Directory under the WAL root holding one marker per uploaded segment.
pub const RAW_UPLOAD_MARKER_SUBDIR: &str = "uploaded";

/// Marker file suffix: `<segment file name>.uploaded`.
const MARKER_SUFFIX: &str = ".uploaded";

/// Scratch directory for in-flight gzip files, under the marker directory.
const TEMP_SUBDIR: &str = ".tmp";

/// Object metadata names (sent as `x-amz-meta-<name>`).
const META_RAW_SHA256: &str = "raw-sha256";
const META_RAW_BYTES: &str = "raw-bytes";

/// Idle wait between passes that found nothing to upload. A session writes a
/// new 128 MiB segment every few minutes, so a minute keeps the copy close
/// behind the writer without spinning.
pub const RAW_UPLOAD_IDLE_SECS: u64 = 60;

/// Wait after each uploaded segment. Measured 2026-09-29: ~186 segments per
/// session, so this clears a full day's backlog in about 16 minutes while
/// leaving most of each interval to the live lane.
pub const RAW_UPLOAD_SPACING_SECS: u64 = 5;

/// Segments a pass may try and skip (growing, conflict, local failure) on top
/// of its upload limit, so a run of bad segments cannot turn one pass into a
/// gzip of the whole backlog.
pub const RAW_UPLOAD_MAX_SKIPS_PER_PASS: usize = 8;

/// A marker whose segment is gone from every WAL directory is removed only
/// once it is this old, so a segment caught mid-rename between directories
/// never loses its marker.
pub const RAW_UPLOAD_ORPHAN_MARKER_MIN_AGE_SECS: u64 = 86_400;

/// Counter: one per segment handled by a pass, labelled `outcome` =
/// `uploaded`, `already_present`, `conflict`, `failed`, `growing`. Local
/// `/metrics` only.
pub const RAW_UPLOAD_COUNTER: &str = "tv_wal_raw_upload_total";

/// Gauge: closed segments with no verified S3 copy after the latest pass.
pub const RAW_UPLOAD_PENDING_GAUGE: &str = "tv_wal_raw_upload_pending_segments";

/// IST is UTC+05:30 all year.
const IST_OFFSET_SECS: i64 = 19_800;

/// Whether a prune must find a verified S3 copy before deleting a segment.
#[derive(Debug, Clone, Copy)]
pub enum RawCopyGate<'a> {
    /// No uploader runs (a box with no explicit environment); the prunes
    /// delete as they did before item 45e.
    NotRequired,
    /// The uploader runs for this WAL root; a segment is deleted only when
    /// [`segment_has_verified_copy`] holds.
    RequireMarker(&'a Path),
}

impl RawCopyGate<'_> {
    /// Whether `segment` (current length `len`) may be deleted as far as the
    /// S3 copy is concerned. Cold prune path.
    #[must_use]
    pub fn has_copy(self, segment: &Path, len: u64) -> bool {
        match self {
            Self::NotRequired => true,
            Self::RequireMarker(root) => segment_has_verified_copy(root, segment, len),
        }
    }

    /// Removes `segment`'s marker after the prune deleted it. Best effort: a
    /// marker left behind is swept by the uploader once it is a day old.
    pub fn forget(self, segment: &Path) {
        if let Self::RequireMarker(root) = self
            && let Some(marker) = marker_path(root, segment)
        {
            // O(1) EXEMPT: cold prune path
            if let Err(err) = std::fs::remove_file(&marker)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                debug!(marker = %marker.display(), error = %err, "raw upload marker not removed");
            }
        }
    }
}

/// What the marker records about one verified upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadMarker {
    /// S3 key of the object.
    pub key: String,
    /// Length of the raw segment that was compressed.
    pub raw_bytes: u64,
    /// SHA-256 of the raw segment, base64. A restore checks the decompressed
    /// bytes against it.
    pub raw_sha256_b64: String,
    /// Length of the gzip object in S3.
    pub gzip_bytes: u64,
    /// SHA-256 of the gzip object in S3, base64 (the `x-amz-checksum-sha256`
    /// form).
    pub gzip_sha256_b64: String,
}

impl UploadMarker {
    /// Marker file body: five `name=value` lines.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "key={}\nraw_bytes={}\nraw_sha256={}\ngzip_bytes={}\ngzip_sha256={}\n",
            self.key, self.raw_bytes, self.raw_sha256_b64, self.gzip_bytes, self.gzip_sha256_b64
        )
    }

    /// Parses a marker body. `None` when any field is missing, malformed or
    /// unknown, which the prunes read as "no copy".
    #[must_use]
    pub fn parse(body: &str) -> Option<Self> {
        let mut key = None;
        let mut raw_bytes = None;
        let mut raw_sha = None;
        let mut gzip_bytes = None;
        let mut sha = None;
        for line in body.lines() {
            let (name, value) = line.split_once('=')?;
            match name {
                "key" => key = Some(value.to_string()),
                "raw_bytes" => raw_bytes = value.parse().ok(),
                "raw_sha256" => raw_sha = Some(value.to_string()),
                "gzip_bytes" => gzip_bytes = value.parse().ok(),
                "gzip_sha256" => sha = Some(value.to_string()),
                _ => return None,
            }
        }
        let marker = Self {
            key: key.filter(|k| !k.is_empty())?,
            raw_bytes: raw_bytes?,
            raw_sha256_b64: raw_sha.filter(|s| !s.is_empty())?,
            gzip_bytes: gzip_bytes.filter(|&n| n > 0)?,
            gzip_sha256_b64: sha.filter(|s| !s.is_empty())?,
        };
        Some(marker)
    }
}

/// `<wal_root>/uploaded/<segment file name>.uploaded`, or `None` when the
/// segment path has no UTF-8 file name.
#[must_use]
pub fn marker_path(wal_root: &Path, segment: &Path) -> Option<PathBuf> {
    let name = segment.file_name()?.to_str()?;
    Some(
        wal_root
            .join(RAW_UPLOAD_MARKER_SUBDIR)
            .join(format!("{name}{MARKER_SUFFIX}")),
    )
}

/// Whether `segment` (current length `len`) has a verified S3 copy: a marker
/// exists, parses, and its recorded raw length is at least the segment's
/// current length. A segment that GREW after its upload has bytes S3 does not
/// hold, so it reads as "no copy". Cold path, one small file read.
#[must_use]
pub fn segment_has_verified_copy(wal_root: &Path, segment: &Path, len: u64) -> bool {
    let Some(marker) = marker_path(wal_root, segment) else {
        return false;
    };
    std::fs::read_to_string(marker) // O(1) EXEMPT: cold prune path, one small file
        .ok()
        .and_then(|body| UploadMarker::parse(&body))
        .is_some_and(|m| m.raw_bytes >= len)
}

/// The IST calendar date (`YYYY-MM-DD`) a segment was opened on, read from
/// its `ws-frames-<nanos>.wal` name. `None` for any other name.
#[must_use]
pub fn ist_date_of_segment(file_name: &str) -> Option<String> {
    let digits = file_name.strip_prefix("ws-frames-")?.strip_suffix(".wal")?;
    let nanos: u128 = digits.parse().ok()?;
    let secs = i64::try_from(nanos / 1_000_000_000).ok()?;
    let ist = chrono::DateTime::from_timestamp(secs.checked_add(IST_OFFSET_SECS)?, 0)?;
    Some(ist.date_naive().format("%Y-%m-%d").to_string())
}

/// S3 key for a segment of `raw_bytes`: `raw-frames/<IST date>/<file
/// name>.<raw bytes>.gz`, or under `raw-frames/undated/` when the name
/// carries no timestamp. The raw length is in the key so a segment that grew
/// after an upload is stored under a new key instead of colliding with the
/// shorter copy.
#[must_use]
pub fn raw_frames_key(file_name: &str, raw_bytes: u64) -> String {
    let date = ist_date_of_segment(file_name).unwrap_or_else(|| "undated".to_string());
    format!("{RAW_FRAMES_S3_PREFIX}/{date}/{file_name}.{raw_bytes}.gz")
}

/// A segment compressed into a local gzip file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedSegment {
    /// Raw bytes read from the segment.
    pub raw_bytes: u64,
    /// SHA-256 of the raw bytes, base64.
    pub raw_sha256_b64: String,
    /// Bytes in the gzip file.
    pub gzip_bytes: u64,
    /// SHA-256 of the gzip file, base64.
    pub gzip_sha256_b64: String,
}

/// gzips `src` into `dst` at level 1, hashing the raw bytes and the gzip
/// stream as they pass. Blocking; call from a blocking thread.
///
/// # Errors
/// Any read, write or fsync failure.
pub fn compress_segment(src: &Path, dst: &Path) -> std::io::Result<CompressedSegment> {
    use sha2::Digest as _;
    let mut input = HashingReader {
        inner: std::fs::File::open(src)?,
        hasher: sha2::Sha256::new(),
    };
    let out = std::fs::File::create(dst)?;
    let hashing = HashingFile {
        inner: std::io::BufWriter::new(out),
        hasher: sha2::Sha256::new(),
        written: 0,
    };
    let mut encoder = GzEncoder::new(hashing, Compression::fast());
    let raw_bytes = std::io::copy(&mut input, &mut encoder)?;
    let mut hashing = encoder.finish()?;
    hashing.inner.flush()?;
    let file = hashing
        .inner
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.sync_all()?;
    let raw_digest: [u8; 32] = input.hasher.finalize().into();
    let digest: [u8; 32] = hashing.hasher.finalize().into();
    Ok(CompressedSegment {
        raw_bytes,
        raw_sha256_b64: crate::partition_archive::base64_encode(&raw_digest),
        gzip_bytes: hashing.written,
        gzip_sha256_b64: crate::partition_archive::base64_encode(&digest),
    })
}

/// `Read` adapter that hashes every byte read.
struct HashingReader {
    inner: std::fs::File,
    hasher: sha2::Sha256,
}

impl std::io::Read for HashingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// `Write` adapter that hashes and counts every byte on its way to the file.
struct HashingFile {
    inner: std::io::BufWriter<std::fs::File>,
    hasher: sha2::Sha256,
    written: u64,
}

impl std::io::Write for HashingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// What a HEAD found under a key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteObject {
    /// Object length.
    pub len: u64,
    /// Stored `x-amz-checksum-sha256`, if any.
    pub sha256_b64: Option<String>,
    /// The raw SHA-256 recorded in the object's metadata, if any.
    pub raw_sha256_b64: Option<String>,
}

/// What S3 holds under a key, against the local gzip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCopy {
    /// No object under the key.
    Missing,
    /// Our copy: the same gzip length and SHA-256, or the same raw SHA-256 in
    /// the metadata (the same raw bytes, compressed by another build).
    Same,
    /// Any other object, including one that carries neither checksum. Never
    /// overwritten; the segment is kept.
    Different,
}

/// Classifies a HEAD result (`None` = no object) against the local gzip.
#[must_use]
pub fn classify_remote(head: Option<&RemoteObject>, local: &CompressedSegment) -> RemoteCopy {
    let Some(remote) = head else {
        return RemoteCopy::Missing;
    };
    let gzip_matches = remote.len == local.gzip_bytes
        && remote
            .sha256_b64
            .as_deref()
            .is_some_and(|s| s.trim() == local.gzip_sha256_b64);
    let raw_matches = remote
        .raw_sha256_b64
        .as_deref()
        .is_some_and(|s| s.trim() == local.raw_sha256_b64);
    if gzip_matches || raw_matches {
        RemoteCopy::Same
    } else {
        RemoteCopy::Different
    }
}

/// Totals from one upload pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUploadPass {
    /// Segments uploaded and verified this pass.
    pub uploaded: usize,
    /// Segments whose copy was already in S3 (marker written).
    pub already_present: usize,
    /// Segments whose key holds a DIFFERENT object; kept, never overwritten.
    pub conflicts: usize,
    /// Segments whose upload or verify failed; retried next pass.
    pub failed: usize,
    /// Segments that changed length or moved while being compressed; retried.
    pub growing: usize,
    /// Closed segments with no verified copy after the pass.
    pub pending: usize,
    /// Orphan markers removed.
    pub orphan_markers_removed: usize,
}

/// The closed segments under a WAL root, from one listing.
#[derive(Debug, Default)]
pub struct SegmentListing {
    /// Regular `*.wal` files in the root, `replaying/` and `archive/`, sorted
    /// by file name (capture order), without the segment the writer holds
    /// open.
    pub segments: Vec<PathBuf>,
    /// `false` when a directory that exists could not be read; the orphan
    /// sweep is skipped then, so a failed listing never reads a live segment's
    /// marker as an orphan.
    pub complete: bool,
}

/// Lists the closed segments under `wal_root` (root, `replaying/`,
/// `archive/`). Symbolic links and other non-regular files are skipped.
/// Blocking.
#[must_use]
pub fn closed_segments(wal_root: &Path) -> SegmentListing {
    // O(1) EXEMPT: cold upload pass, one entry per segment
    let mut out = SegmentListing {
        segments: Vec::with_capacity(256),
        complete: true,
    };
    for dir in [
        wal_root.to_path_buf(),
        wal_root.join("replaying"),
        wal_root.join("archive"),
    ] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                out.complete = false;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                out.complete = false;
                continue;
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wal")
                && entry.file_type().is_ok_and(|t| t.is_file())
                && !crate::ws_frame_spill::is_open_segment(&path)
            {
                out.segments.push(path);
            }
        }
    }
    out.segments
        .sort_by(|a, b| a.file_name().cmp(&b.file_name())); // O(1) EXEMPT: cold upload pass
    out
}

/// Writes `marker` for `segment` atomically: temporary file, fsync, rename.
/// Blocking.
///
/// # Errors
/// Any create, write, fsync or rename failure.
pub fn write_marker(wal_root: &Path, segment: &Path, marker: &UploadMarker) -> std::io::Result<()> {
    let Some(path) = marker_path(wal_root, segment) else {
        return Err(std::io::Error::other("segment has no UTF-8 file name"));
    };
    let dir = wal_root.join(RAW_UPLOAD_MARKER_SUBDIR);
    let tmp = dir.join(TEMP_SUBDIR);
    std::fs::create_dir_all(&tmp)?;
    let tmp_file = tmp.join(format!(
        "{}.marker-tmp",
        segment
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("segment")
    ));
    {
        let mut f = std::fs::File::create(&tmp_file)?;
        f.write_all(marker.encode().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_file, &path)
}

/// Removes markers whose segment is in none of the WAL directories and that
/// are at least [`RAW_UPLOAD_ORPHAN_MARKER_MIN_AGE_SECS`] old. Only regular
/// files are removed. Returns how many were removed. Blocking.
pub fn sweep_orphan_markers(
    wal_root: &Path,
    live_segment_names: &HashSet<String>,
    now: std::time::SystemTime,
) -> usize {
    let Ok(entries) = std::fs::read_dir(wal_root.join(RAW_UPLOAD_MARKER_SUBDIR)) else {
        return 0;
    };
    let min_age = Duration::from_secs(RAW_UPLOAD_ORPHAN_MARKER_MIN_AGE_SECS);
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(segment_name) = name.strip_suffix(MARKER_SUFFIX) else {
            continue;
        };
        if live_segment_names.contains(segment_name) {
            continue;
        }
        let old_enough = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mt| now.duration_since(mt).ok())
            .is_some_and(|age| age >= min_age);
        if old_enough && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Removes leftover `*.gz` files from the scratch directory (a pass that died
/// between compressing and cleaning up). Only regular files. Blocking.
fn remove_scratch_gzips(wal_root: &Path) {
    let Ok(entries) = std::fs::read_dir(wal_root.join(RAW_UPLOAD_MARKER_SUBDIR).join(TEMP_SUBDIR))
    else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("gz")
            && entry.file_type().is_ok_and(|t| t.is_file())
            && let Err(err) = std::fs::remove_file(&path)
        {
            warn!(path = %path.display(), error = %err, "raw upload scratch gzip not removed");
        }
    }
}

/// One segment the scan found without a verified copy.
#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    name: String,
    len: u64,
}

/// What the blocking scan hands to the async pass.
#[derive(Debug, Default)]
struct Scan {
    candidates: Vec<Candidate>,
    live_names: HashSet<String>,
    complete: bool,
}

/// Clears stale scratch files, lists the closed segments and keeps those
/// without a verified copy. Blocking.
fn scan_for_candidates(wal_root: &Path) -> Scan {
    // One uploader per WAL root, so any gzip left in the scratch directory is
    // from a pass that died; it is rebuilt on demand.
    remove_scratch_gzips(wal_root);
    let listing = closed_segments(wal_root);
    let mut scan = Scan {
        candidates: Vec::with_capacity(listing.segments.len()),
        live_names: HashSet::with_capacity(listing.segments.len()),
        complete: listing.complete,
    };
    for path in listing.segments {
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        scan.live_names.insert(name.clone());
        let Ok(len) = std::fs::metadata(&path).map(|m| m.len()) else {
            continue; // moved or deleted under us; the next pass sees it
        };
        if !segment_has_verified_copy(wal_root, &path, len) {
            scan.candidates.push(Candidate { path, name, len });
        }
    }
    scan
}

/// Why one segment's upload did not produce a marker.
#[derive(Debug)]
enum SegmentUploadError {
    /// The segment changed length or moved while it was compressed.
    Growing,
    /// The key holds a different object.
    Conflict,
    /// Local I/O failed; the pass moves on to the next segment.
    Local(anyhow::Error),
    /// An S3 call or the read-back verify failed; the pass stops.
    Remote(anyhow::Error),
}

/// Runs blocking file work off the async worker threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SegmentUploadError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| SegmentUploadError::Local(anyhow::anyhow!("blocking task: {e}")))
}

/// The cold bucket for this process, or `None` when no environment is set
/// explicitly (`TV_ENVIRONMENT` / `ENVIRONMENT`): the same fail-closed bucket
/// rule the partition archive uses, so a dev box never writes into a
/// production bucket.
#[must_use]
// TEST-EXEMPT: reads the process environment; the rule is tested via `resolve_archive_bucket` / `resolve_environment_from`
pub fn configured_raw_frames_bucket() -> Option<String> {
    let env = crate::partition_archive::runtime_environment()?;
    crate::partition_archive::resolve_archive_bucket("", Some(&env))
}

/// Uploads closed WAL segments to `s3://<bucket>/raw-frames/` and writes the
/// markers the prunes require (item 45e).
pub struct RawFrameUploader {
    s3: aws_sdk_s3::Client,
    bucket: String,
    /// Segments found in conflict this process; skipped quietly after the
    /// first coded error. Pruned to the segments still on disk every pass.
    conflicted: HashSet<String>,
}

impl RawFrameUploader {
    /// Loads the AWS configuration and builds the uploader for `bucket`.
    // TEST-EXEMPT: loads the ambient AWS configuration; the uploader itself is tested via `from_parts` against a stub S3
    pub async fn connect(bucket: String) -> Self {
        let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        Self::from_parts(aws_sdk_s3::Client::new(&aws_conf), bucket)
    }

    /// Builds the uploader over an explicit S3 client and bucket.
    #[must_use]
    pub fn from_parts(s3: aws_sdk_s3::Client, bucket: String) -> Self {
        Self {
            s3,
            bucket,
            conflicted: HashSet::new(),
        }
    }

    /// The bucket this uploader writes to.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Runs passes forever: after a pass that uploaded something the next one
    /// starts after [`RAW_UPLOAD_SPACING_SECS`], otherwise after
    /// [`RAW_UPLOAD_IDLE_SECS`].
    // TEST-EXEMPT: an infinite loop over `upload_pass`, which is tested against a stub S3
    pub async fn run_forever(mut self, wal_root: PathBuf) {
        info!(
            bucket = %self.bucket,
            prefix = RAW_FRAMES_S3_PREFIX,
            wal_dir = %wal_root.display(),
            "raw WAL segment uploader started; the WAL prunes now delete only segments with a \
             verified S3 copy"
        );
        loop {
            let pass = self.upload_pass(&wal_root, 1).await;
            let wait = if pass.uploaded + pass.already_present > 0 {
                RAW_UPLOAD_SPACING_SECS
            } else {
                RAW_UPLOAD_IDLE_SECS
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    }

    /// One pass: copies up to `max_uploads` segments without a verified copy,
    /// oldest first, then sweeps orphan markers and publishes the pending
    /// gauge. A growing, conflicting or locally failing segment is skipped
    /// (at most [`RAW_UPLOAD_MAX_SKIPS_PER_PASS`] per pass) and the pass moves
    /// on; an S3 failure stops it, and the next pass retries.
    pub async fn upload_pass(&mut self, wal_root: &Path, max_uploads: usize) -> RawUploadPass {
        let mut pass = RawUploadPass::default();
        let root = wal_root.to_path_buf();
        let Ok(scan) = blocking(move || scan_for_candidates(&root)).await else {
            return pass;
        };
        self.conflicted
            .retain(|name| scan.live_names.contains(name));
        let mut copied = 0usize;
        let mut skipped = 0usize;
        let mut s3_down = false;
        for c in &scan.candidates {
            if self.conflicted.contains(&c.name) {
                pass.conflicts += 1;
                pass.pending += 1;
                continue;
            }
            if s3_down || copied >= max_uploads || skipped >= RAW_UPLOAD_MAX_SKIPS_PER_PASS {
                pass.pending += 1;
                continue;
            }
            match self.upload_segment(wal_root, c).await {
                Ok(RemoteCopy::Same) => {
                    copied += 1;
                    pass.already_present += 1;
                    metrics::counter!(RAW_UPLOAD_COUNTER, "outcome" => "already_present")
                        .increment(1);
                }
                Ok(_) => {
                    copied += 1;
                    pass.uploaded += 1;
                    metrics::counter!(RAW_UPLOAD_COUNTER, "outcome" => "uploaded").increment(1);
                }
                Err(SegmentUploadError::Growing) => {
                    skipped += 1;
                    pass.growing += 1;
                    pass.pending += 1;
                    metrics::counter!(RAW_UPLOAD_COUNTER, "outcome" => "growing").increment(1);
                }
                Err(SegmentUploadError::Conflict) => {
                    skipped += 1;
                    pass.conflicts += 1;
                    pass.pending += 1;
                    self.conflicted.insert(c.name.clone());
                    metrics::counter!(RAW_UPLOAD_COUNTER, "outcome" => "conflict").increment(1);
                    error!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "raw_frames_conflict",
                        segment = %c.path.display(),
                        key = %raw_frames_key(&c.name, c.len),
                        "a raw WAL segment's S3 key already holds a DIFFERENT object. It is never \
                         overwritten, so this segment has no verified copy and the cleanup will \
                         keep it on disk. Logged once per process; compare the two by hand."
                    );
                }
                Err(SegmentUploadError::Local(err)) => {
                    skipped += 1;
                    pass.failed += 1;
                    pass.pending += 1;
                    metrics::counter!(RAW_UPLOAD_COUNTER, "outcome" => "failed").increment(1);
                    error!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "raw_frames_upload",
                        segment = %c.path.display(),
                        error = %err,
                        "raw WAL segment could not be prepared for upload (local disk); the \
                         uploader moved on to the next segment and retries this one next pass. \
                         Until it succeeds the segment has no copy and the cleanup keeps it on \
                         disk."
                    );
                }
                Err(SegmentUploadError::Remote(err)) => {
                    s3_down = true;
                    pass.failed += 1;
                    pass.pending += 1;
                    metrics::counter!(RAW_UPLOAD_COUNTER, "outcome" => "failed").increment(1);
                    error!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "raw_frames_upload",
                        segment = %c.path.display(),
                        error = %err,
                        "raw WAL segment upload to S3 failed; retried next pass. Until it \
                         succeeds the segment has no copy and the cleanup keeps it on disk."
                    );
                }
            }
        }
        if scan.complete {
            let root = wal_root.to_path_buf();
            let live = scan.live_names;
            pass.orphan_markers_removed =
                blocking(move || sweep_orphan_markers(&root, &live, std::time::SystemTime::now()))
                    .await
                    .unwrap_or(0);
        }
        // APPROVED: cast — a segment count, far below f64 precision loss.
        #[allow(clippy::cast_precision_loss)] // APPROVED: segment count
        metrics::gauge!(RAW_UPLOAD_PENDING_GAUGE).set(pass.pending as f64);
        if pass.uploaded + pass.already_present + pass.failed + pass.conflicts > 0 {
            debug!(?pass, "raw WAL upload pass");
        }
        pass
    }

    /// Compresses, uploads and verifies one segment, then writes its marker.
    /// Returns `Missing` after a fresh upload and `Same` when our copy was
    /// already there.
    async fn upload_segment(
        &self,
        wal_root: &Path,
        c: &Candidate,
    ) -> Result<RemoteCopy, SegmentUploadError> {
        let gz_path = wal_root
            .join(RAW_UPLOAD_MARKER_SUBDIR)
            .join(TEMP_SUBDIR)
            .join(format!("{}.gz", c.name));
        let result = self.upload_compressed(wal_root, c, &gz_path).await;
        let gz = gz_path.clone();
        let cleanup = blocking(move || std::fs::remove_file(&gz)).await;
        if let Ok(Err(err)) = cleanup
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %gz_path.display(), error = %err, "raw upload scratch gzip not removed");
        }
        result
    }

    async fn upload_compressed(
        &self,
        wal_root: &Path,
        c: &Candidate,
        gz_path: &Path,
    ) -> Result<RemoteCopy, SegmentUploadError> {
        let (src, dst) = (c.path.clone(), gz_path.to_path_buf());
        let len_before = c.len;
        let compressed = blocking(move || -> Result<_, SegmentUploadError> {
            if let Some(dir) = dst.parent() {
                std::fs::create_dir_all(dir).map_err(|e| {
                    SegmentUploadError::Local(anyhow::anyhow!("create {dir:?}: {e}"))
                })?;
            }
            let compressed = match compress_segment(&src, &dst) {
                Ok(c) => c,
                // Renamed between directories by replay staging or confirm.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SegmentUploadError::Growing);
                }
                Err(e) => return Err(SegmentUploadError::Local(anyhow::anyhow!("compress: {e}"))),
            };
            match std::fs::metadata(&src) {
                Ok(m) if m.len() == len_before && compressed.raw_bytes == len_before => {
                    Ok(compressed)
                }
                Ok(_) => Err(SegmentUploadError::Growing),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(SegmentUploadError::Growing)
                }
                Err(e) => Err(SegmentUploadError::Local(anyhow::anyhow!(
                    "stat segment: {e}"
                ))),
            }
        })
        .await??;
        let key = raw_frames_key(&c.name, compressed.raw_bytes);
        let before = self.head(&key).await.map_err(SegmentUploadError::Remote)?;
        let first = classify_remote(before.as_ref(), &compressed);
        let held = match first {
            RemoteCopy::Different => return Err(SegmentUploadError::Conflict),
            RemoteCopy::Same => before,
            RemoteCopy::Missing => {
                if let Err(err) = self.put(&key, gz_path, &compressed).await {
                    // A lost race or a retried PUT that landed: HEAD decides.
                    warn!(key = %key, error = %err, "raw WAL PUT failed; checking S3 before retry");
                }
                let after = self.head(&key).await.map_err(SegmentUploadError::Remote)?;
                match classify_remote(after.as_ref(), &compressed) {
                    RemoteCopy::Same => after,
                    RemoteCopy::Different => return Err(SegmentUploadError::Conflict),
                    RemoteCopy::Missing => {
                        return Err(SegmentUploadError::Remote(anyhow::anyhow!(
                            "object {key} not in S3 after PUT"
                        )));
                    }
                }
            }
        };
        // The marker records what S3 holds; a copy matched by its raw hash may
        // be a different gzip of the same bytes.
        let held = held.unwrap_or_default();
        let marker = UploadMarker {
            key,
            raw_bytes: compressed.raw_bytes,
            raw_sha256_b64: compressed.raw_sha256_b64.clone(),
            gzip_bytes: if held.len > 0 {
                held.len
            } else {
                compressed.gzip_bytes
            },
            gzip_sha256_b64: held
                .sha256_b64
                .unwrap_or_else(|| compressed.gzip_sha256_b64.clone()),
        };
        let (root, segment) = (wal_root.to_path_buf(), c.path.clone());
        blocking(move || write_marker(&root, &segment, &marker))
            .await?
            .map_err(|e| SegmentUploadError::Local(anyhow::anyhow!("write marker: {e}")))?;
        Ok(first)
    }

    /// HEAD with the stored SHA-256 requested. `Ok(None)` = no object.
    async fn head(&self, key: &str) -> anyhow::Result<Option<RemoteObject>> {
        match self
            .s3
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
        {
            Ok(head) => {
                let nonempty = |s: &str| {
                    let s = s.trim();
                    (!s.is_empty()).then(|| s.to_string())
                };
                Ok(Some(RemoteObject {
                    len: head
                        .content_length()
                        .and_then(|n| u64::try_from(n).ok())
                        .unwrap_or(0),
                    sha256_b64: head.checksum_sha256().and_then(nonempty),
                    raw_sha256_b64: head
                        .metadata()
                        .and_then(|m| m.get(META_RAW_SHA256))
                        .and_then(|s| nonempty(s)),
                }))
            }
            Err(err) => {
                let not_found = err
                    .as_service_error()
                    .is_some_and(aws_sdk_s3::operation::head_object::HeadObjectError::is_not_found);
                if not_found {
                    Ok(None)
                } else {
                    Err(anyhow::anyhow!("S3 HeadObject {key}: {err}"))
                }
            }
        }
    }

    /// Conditional create with the SHA-256 S3 validates server side and the
    /// raw hash and length as metadata.
    async fn put(&self, key: &str, path: &Path, local: &CompressedSegment) -> anyhow::Result<()> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(path)
            .await
            .map_err(|e| anyhow::anyhow!("open gzip for upload: {e}"))?;
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .checksum_sha256(&local.gzip_sha256_b64)
            .metadata(META_RAW_SHA256, &local.raw_sha256_b64)
            .metadata(META_RAW_BYTES, local.raw_bytes.to_string())
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 PutObject {key}: {e}"))?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::io::Read as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-raw-upload-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// 2026-10-01 10:00:00 IST = 04:30:00 UTC.
    const NANOS_2026_10_01_1000_IST: u128 = 1_790_829_000 * 1_000_000_000;

    fn segment_name(nanos: u128) -> String {
        format!("ws-frames-{nanos:020}.wal")
    }

    fn plant_segment(dir: &Path, nanos: u128, body: &[u8]) -> PathBuf {
        let path = dir.join(segment_name(nanos));
        std::fs::write(&path, body).expect("write segment");
        path
    }

    fn sample_marker() -> UploadMarker {
        UploadMarker {
            key: "raw-frames/2026-10-01/ws-frames-1.wal.1000.gz".to_string(),
            raw_bytes: 1000,
            raw_sha256_b64: "raw=".to_string(),
            gzip_bytes: 400,
            gzip_sha256_b64: "abc=".to_string(),
        }
    }

    #[test]
    fn marker_round_trips_and_rejects_partial_bodies() {
        let m = sample_marker();
        assert_eq!(UploadMarker::parse(&m.encode()), Some(m.clone()));
        assert_eq!(UploadMarker::parse(""), None);
        assert_eq!(UploadMarker::parse("garbage"), None);
        assert_eq!(
            UploadMarker::parse("key=a\nraw_bytes=1\nraw_sha256=r\ngzip_bytes=1\n"),
            None
        );
        assert_eq!(
            UploadMarker::parse("key=a\nraw_bytes=1\ngzip_bytes=1\ngzip_sha256=s\n"),
            None,
            "a marker without the raw hash is not a copy"
        );
        assert_eq!(
            UploadMarker::parse("key=a\nraw_bytes=x\nraw_sha256=r\ngzip_bytes=1\ngzip_sha256=s\n"),
            None
        );
        assert_eq!(
            UploadMarker::parse("key=a\nraw_bytes=1\nraw_sha256=r\ngzip_bytes=0\ngzip_sha256=s\n"),
            None,
            "an empty gzip object is not a copy"
        );
        assert_eq!(
            UploadMarker::parse(&format!("{}extra=1\n", m.encode())),
            None,
            "an unknown field is rejected, not ignored"
        );
    }

    #[test]
    fn test_write_marker_then_segment_has_verified_copy_needs_the_current_length() {
        let dir = tmp_dir("verified");
        let seg = plant_segment(&dir, 1, &[7u8; 1000]);
        assert!(!segment_has_verified_copy(&dir, &seg, 1000), "no marker");
        write_marker(&dir, &seg, &sample_marker()).expect("marker");
        assert!(segment_has_verified_copy(&dir, &seg, 1000));
        assert!(
            segment_has_verified_copy(&dir, &seg, 900),
            "a truncated segment is covered"
        );
        assert!(
            !segment_has_verified_copy(&dir, &seg, 1001),
            "a grown segment is not"
        );
        let marker = marker_path(&dir, &seg).expect("marker path");
        std::fs::write(&marker, "corrupt").expect("corrupt marker");
        assert!(
            !segment_has_verified_copy(&dir, &seg, 1),
            "a corrupt marker is no copy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_has_copy_and_forget_follow_the_marker() {
        let dir = tmp_dir("gate");
        let seg = plant_segment(&dir, 2, b"x");
        assert!(RawCopyGate::NotRequired.has_copy(&seg, u64::MAX));
        let gate = RawCopyGate::RequireMarker(&dir);
        assert!(!gate.has_copy(&seg, 1));
        write_marker(&dir, &seg, &sample_marker()).expect("marker");
        assert!(gate.has_copy(&seg, 1));
        gate.forget(&seg);
        assert!(!gate.has_copy(&seg, 1));
        RawCopyGate::NotRequired.forget(&seg); // no-op, no panic
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ist_date_of_segment_and_raw_frames_key_come_from_the_name() {
        let name = segment_name(NANOS_2026_10_01_1000_IST);
        assert_eq!(ist_date_of_segment(&name).as_deref(), Some("2026-10-01"));
        assert_eq!(
            raw_frames_key(&name, 42),
            format!("raw-frames/2026-10-01/{name}.42.gz")
        );
        assert_ne!(
            raw_frames_key(&name, 42),
            raw_frames_key(&name, 43),
            "a segment that grew gets a new key"
        );
        // 2026-09-30 19:00 UTC is 00:30 IST on 2026-10-01.
        let late = segment_name(1_790_794_800 * 1_000_000_000);
        assert_eq!(ist_date_of_segment(&late).as_deref(), Some("2026-10-01"));
        // 2026-09-30 18:29:59 UTC is 23:59:59 IST on 2026-09-30.
        let before_midnight = segment_name(1_790_792_999 * 1_000_000_000);
        assert_eq!(
            ist_date_of_segment(&before_midnight).as_deref(),
            Some("2026-09-30")
        );
        for bad in ["seg-001.wal", "ws-frames-abc.wal", "ws-frames-1.txt", ""] {
            assert_eq!(ist_date_of_segment(bad), None, "{bad}");
        }
        assert_eq!(
            raw_frames_key("seg-001.wal", 7),
            "raw-frames/undated/seg-001.wal.7.gz"
        );
    }

    #[test]
    fn compress_segment_round_trips_and_hashes_the_raw_and_gzip_bytes() {
        use sha2::Digest as _;
        let dir = tmp_dir("gzip");
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let seg = plant_segment(&dir, 3, &body);
        let gz = dir.join("out.gz");
        let c = compress_segment(&seg, &gz).expect("compress");
        let on_disk = std::fs::read(&gz).expect("read gz");
        assert_eq!(c.raw_bytes, body.len() as u64);
        assert_eq!(c.gzip_bytes, on_disk.len() as u64);
        let digest: [u8; 32] = sha2::Sha256::digest(&on_disk).into();
        assert_eq!(
            c.gzip_sha256_b64,
            crate::partition_archive::base64_encode(&digest)
        );
        let raw_digest: [u8; 32] = sha2::Sha256::digest(&body).into();
        assert_eq!(
            c.raw_sha256_b64,
            crate::partition_archive::base64_encode(&raw_digest)
        );
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(on_disk.as_slice())
            .read_to_end(&mut back)
            .expect("gunzip");
        assert_eq!(back, body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_remote_requires_the_same_gzip_or_the_same_raw_bytes() {
        let local = CompressedSegment {
            raw_bytes: 100,
            raw_sha256_b64: "r".into(),
            gzip_bytes: 10,
            gzip_sha256_b64: "s".into(),
        };
        let remote = |len: u64, sha: Option<&str>, raw: Option<&str>| RemoteObject {
            len,
            sha256_b64: sha.map(str::to_string),
            raw_sha256_b64: raw.map(str::to_string),
        };
        assert_eq!(classify_remote(None, &local), RemoteCopy::Missing);
        assert_eq!(
            classify_remote(Some(&remote(10, Some("s"), None)), &local),
            RemoteCopy::Same
        );
        assert_eq!(
            classify_remote(Some(&remote(10, Some(" s "), None)), &local),
            RemoteCopy::Same
        );
        assert_eq!(
            classify_remote(Some(&remote(11, Some("s"), None)), &local),
            RemoteCopy::Different
        );
        assert_eq!(
            classify_remote(Some(&remote(10, Some("t"), None)), &local),
            RemoteCopy::Different
        );
        assert_eq!(
            classify_remote(Some(&remote(10, None, None)), &local),
            RemoteCopy::Different,
            "an object without a checksum cannot be proven to be ours"
        );
        assert_eq!(
            classify_remote(Some(&remote(12, Some("other-gzip"), Some("r"))), &local),
            RemoteCopy::Same,
            "the same raw bytes compressed by another build are our copy"
        );
        assert_eq!(
            classify_remote(Some(&remote(12, Some("other-gzip"), Some("x"))), &local),
            RemoteCopy::Different
        );
    }

    #[test]
    fn test_sweep_orphan_markers_removes_only_day_old_orphans() {
        let dir = tmp_dir("orphans");
        let live = plant_segment(&dir, 10, b"x");
        let gone_old = dir.join(segment_name(11));
        let gone_young = dir.join(segment_name(12));
        for seg in [&live, &gone_old, &gone_young] {
            write_marker(&dir, seg, &sample_marker()).expect("marker");
        }
        let old_marker = marker_path(&dir, &gone_old).expect("path");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&old_marker)
            .expect("open");
        f.set_times(std::fs::FileTimes::new().set_modified(
            SystemTime::now() - Duration::from_secs(RAW_UPLOAD_ORPHAN_MARKER_MIN_AGE_SECS + 60),
        ))
        .expect("backdate");
        let names: HashSet<String> = [segment_name(10)].into_iter().collect();
        assert_eq!(sweep_orphan_markers(&dir, &names, SystemTime::now()), 1);
        assert!(!old_marker.exists());
        assert!(marker_path(&dir, &gone_young).is_some_and(|p| p.exists()));
        assert!(marker_path(&dir, &live).is_some_and(|p| p.exists()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closed_segments_lists_all_three_directories_in_capture_order_and_skips_others() {
        let dir = tmp_dir("listing");
        std::fs::create_dir_all(dir.join("replaying")).expect("mkdir");
        std::fs::create_dir_all(dir.join("archive")).expect("mkdir");
        let a = plant_segment(&dir.join("archive"), 1, b"a");
        let b = plant_segment(&dir.join("replaying"), 2, b"b");
        let c = plant_segment(&dir, 3, b"c");
        std::fs::write(dir.join("notes.txt"), b"n").expect("foreign");
        let outside = tmp_dir("listing-outside");
        let target = plant_segment(&outside, 4, b"t");
        std::os::unix::fs::symlink(&target, dir.join(segment_name(5))).expect("symlink");
        let listing = closed_segments(&dir);
        assert_eq!(
            listing.segments,
            vec![a, b, c],
            "a symbolic link is skipped"
        );
        assert!(listing.complete);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn test_closed_segments_reports_an_unreadable_directory_as_incomplete() {
        let dir = tmp_dir("listing-bad");
        // A FILE where `replaying/` should be: read_dir fails with an error
        // other than NotFound.
        std::fs::write(dir.join("replaying"), b"not a dir").expect("file");
        let listing = closed_segments(&dir);
        assert!(!listing.complete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_open_segment_keeps_the_writers_segment_out_of_closed_segments() {
        let dir = tmp_dir("open");
        let spill = crate::ws_frame_spill::WsFrameSpill::new(&dir).expect("spill");
        // The writer opens its first segment at start; wait for it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && std::fs::read_dir(&dir)
                .map(|d| {
                    d.flatten()
                        .filter(|e| e.path().extension().is_some_and(|x| x == "wal"))
                        .count()
                })
                .unwrap_or(0)
                == 0
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            closed_segments(&dir).segments.is_empty(),
            "the open segment is still being written and must not be uploaded"
        );
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- stub S3 ----------------------------------------------------------

    /// What the stub S3 does.
    #[derive(Clone, Copy, Default)]
    struct StubMode {
        /// Every HEAD answers 500.
        head_fails: bool,
        /// PUT stores this length instead of the body's (a bad upload).
        put_len_override: Option<usize>,
    }

    /// Stored object: (length, sha256 b64, raw sha256 metadata).
    type Objects = Arc<Mutex<HashMap<String, (usize, Option<String>, Option<String>)>>>;
    type Log = Arc<Mutex<Vec<(String, String, Vec<(String, String)>)>>>;

    fn find(h: &[u8], n: &[u8]) -> Option<usize> {
        h.windows(n.len()).position(|w| w == n)
    }

    /// Minimal path-style S3 on 127.0.0.1: HEAD and PUT on `/bucket/key`.
    async fn spawn_s3(objects: Objects, mode: StubMode) -> (String, Log) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let accept_log = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let objects = Arc::clone(&objects);
                let log = Arc::clone(&accept_log);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 16_384];
                    loop {
                        let end = loop {
                            if let Some(p) = find(&buf, b"\r\n\r\n") {
                                break p + 4;
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let mut lines = head.split("\r\n");
                        let mut first = lines.next().unwrap_or_default().split(' ');
                        let method = first.next().unwrap_or_default().to_string();
                        let target = first.next().unwrap_or_default().to_string();
                        let mut headers = Vec::new();
                        let mut clen = 0usize;
                        for line in lines {
                            if let Some((k, v)) = line.split_once(':') {
                                let k = k.trim().to_ascii_lowercase();
                                let v = v.trim().to_string();
                                if k == "content-length" {
                                    clen = v.parse().unwrap_or(0);
                                }
                                headers.push((k, v));
                            }
                        }
                        let mut body = buf.split_off(end);
                        buf.clear();
                        while body.len() < clen {
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => body.extend_from_slice(&tmp[..n]),
                            }
                        }
                        buf = body.split_off(clen.min(body.len()));
                        let key = target
                            .split('?')
                            .next()
                            .unwrap_or_default()
                            .trim_start_matches('/')
                            .to_string();
                        let header = |name: &str| {
                            headers
                                .iter()
                                .find(|(k, _)| k == name)
                                .map(|(_, v)| v.clone())
                        };
                        log.lock().expect("log").push((
                            method.clone(),
                            key.clone(),
                            headers.clone(),
                        ));
                        let (status, extra): (u16, Vec<(String, String)>) = match method.as_str() {
                            "HEAD" if mode.head_fails => {
                                (500, vec![("content-length".into(), "0".into())])
                            }
                            "HEAD" => match objects.lock().expect("objects").get(&key) {
                                Some((len, sum, raw)) => {
                                    let mut h = vec![("content-length".into(), len.to_string())];
                                    if let Some(s) = sum {
                                        h.push(("x-amz-checksum-sha256".into(), s.clone()));
                                    }
                                    if let Some(r) = raw {
                                        h.push(("x-amz-meta-raw-sha256".into(), r.clone()));
                                    }
                                    (200, h)
                                }
                                None => (404, vec![("content-length".into(), "0".into())]),
                            },
                            "PUT" => {
                                let mut map = objects.lock().expect("objects");
                                if map.contains_key(&key) && header("if-none-match").is_some() {
                                    (412, Vec::new())
                                } else {
                                    let stored = match mode.put_len_override {
                                        // A bad upload: wrong length, wrong
                                        // checksum, metadata lost.
                                        Some(len) => (len, Some("corrupt".to_string()), None),
                                        None => (
                                            clen,
                                            header("x-amz-checksum-sha256"),
                                            header("x-amz-meta-raw-sha256"),
                                        ),
                                    };
                                    map.insert(key, stored);
                                    (200, vec![("etag".into(), "\"stub\"".into())])
                                }
                            }
                            _ => (500, Vec::new()),
                        };
                        let mut resp = format!("HTTP/1.1 {status} Stub\r\n");
                        if !extra.iter().any(|(k, _)| k == "content-length") {
                            resp.push_str("content-length: 0\r\n");
                        }
                        for (k, v) in &extra {
                            resp.push_str(&format!("{k}: {v}\r\n"));
                        }
                        resp.push_str("\r\n");
                        if sock.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), log)
    }

    fn uploader(url: &str) -> RawFrameUploader {
        let creds = aws_sdk_s3::config::Credentials::new("test", "test", None, None, "stub");
        let conf = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("ap-south-1"))
            .endpoint_url(url)
            .credentials_provider(creds)
            .force_path_style(true)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .build();
        RawFrameUploader::from_parts(aws_sdk_s3::Client::from_conf(conf), "tv-test-cold".into())
    }

    fn seg_key(nanos: u128, raw_bytes: u64) -> String {
        format!(
            "tv-test-cold/{}",
            raw_frames_key(&segment_name(nanos), raw_bytes)
        )
    }

    fn methods_for(log: &Log, name: &str) -> Vec<String> {
        log.lock()
            .expect("log")
            .iter()
            .filter(|(_, k, _)| k.contains(name))
            .map(|(m, _, _)| m.clone())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_pass_uploads_verifies_and_marks_then_has_nothing_left() {
        let dir = tmp_dir("pass");
        let seg = plant_segment(&dir, NANOS_2026_10_01_1000_IST, &[42u8; 50_000]);
        let objects: Objects = Arc::default();
        let (url, log) = spawn_s3(Arc::clone(&objects), StubMode::default()).await;
        let mut up = uploader(&url);
        assert_eq!(up.bucket(), "tv-test-cold");
        let pass = up.upload_pass(&dir, 10).await;
        assert_eq!(pass.uploaded, 1, "{pass:?}");
        assert_eq!(pass.pending, 0);
        assert!(segment_has_verified_copy(&dir, &seg, 50_000));
        let stored = objects
            .lock()
            .expect("objects")
            .get(&seg_key(NANOS_2026_10_01_1000_IST, 50_000))
            .cloned();
        assert!(
            stored.is_some_and(|(_, sum, raw)| sum.is_some() && raw.is_some()),
            "stored with its checksum and the raw hash"
        );
        {
            let log = log.lock().expect("log");
            let put = log.iter().find(|(m, _, _)| m == "PUT").expect("one PUT");
            assert!(
                put.2.iter().any(|(k, v)| k == "if-none-match" && v == "*"),
                "the PUT is a no-overwrite create"
            );
            assert!(put.2.iter().any(|(k, _)| k == "x-amz-checksum-sha256"));
            assert!(
                put.2
                    .iter()
                    .any(|(k, v)| k == "x-amz-meta-raw-bytes" && v == "50000")
            );
        }
        let marker = std::fs::read_to_string(marker_path(&dir, &seg).expect("path"))
            .ok()
            .and_then(|b| UploadMarker::parse(&b))
            .expect("marker");
        assert_eq!(marker.raw_bytes, 50_000);
        let again = up.upload_pass(&dir, 10).await;
        assert_eq!(again, RawUploadPass::default(), "nothing left to do");
        assert!(
            !dir.join(RAW_UPLOAD_MARKER_SUBDIR)
                .join(TEMP_SUBDIR)
                .join(format!("{}.gz", segment_name(NANOS_2026_10_01_1000_IST)))
                .exists(),
            "the local gzip is removed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_identical_object_already_in_s3_is_marked_without_a_second_put() {
        let dir = tmp_dir("present");
        let body = [9u8; 30_000];
        let seg = plant_segment(&dir, 5, &body);
        let gz = dir.join("probe.gz");
        let c = compress_segment(&seg, &gz).expect("compress");
        std::fs::remove_file(&gz).expect("rm probe");
        let objects: Objects = Arc::default();
        objects.lock().expect("objects").insert(
            seg_key(5, 30_000),
            (c.gzip_bytes as usize, Some(c.gzip_sha256_b64.clone()), None),
        );
        let (url, log) = spawn_s3(Arc::clone(&objects), StubMode::default()).await;
        let pass = uploader(&url).upload_pass(&dir, 10).await;
        assert_eq!(pass.already_present, 1, "{pass:?}");
        assert!(segment_has_verified_copy(&dir, &seg, body.len() as u64));
        assert!(
            !log.lock().expect("log").iter().any(|(m, _, _)| m == "PUT"),
            "never re-uploaded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_copy_with_the_same_raw_hash_but_another_gzip_is_ours() {
        use sha2::Digest as _;
        let dir = tmp_dir("rawmatch");
        let body = [4u8; 12_000];
        let seg = plant_segment(&dir, 30, &body);
        let raw: [u8; 32] = sha2::Sha256::digest(body).into();
        let raw_b64 = crate::partition_archive::base64_encode(&raw);
        let objects: Objects = Arc::default();
        objects.lock().expect("objects").insert(
            seg_key(30, 12_000),
            (999, Some("another-build".into()), Some(raw_b64)),
        );
        let (url, log) = spawn_s3(Arc::clone(&objects), StubMode::default()).await;
        let pass = uploader(&url).upload_pass(&dir, 10).await;
        assert_eq!(pass.already_present, 1, "{pass:?}");
        assert!(!log.lock().expect("log").iter().any(|(m, _, _)| m == "PUT"));
        let marker = std::fs::read_to_string(marker_path(&dir, &seg).expect("path"))
            .ok()
            .and_then(|b| UploadMarker::parse(&b))
            .expect("marker");
        assert_eq!(
            (marker.gzip_bytes, marker.gzip_sha256_b64.as_str()),
            (999, "another-build"),
            "the marker records what S3 holds"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_conflict_is_kept_skipped_and_the_pass_moves_on() {
        let dir = tmp_dir("conflict");
        let seg = plant_segment(&dir, 6, &[1u8; 10_000]);
        let next = plant_segment(&dir, 7, &[2u8; 10_000]);
        let objects: Objects = Arc::default();
        objects
            .lock()
            .expect("objects")
            .insert(seg_key(6, 10_000), (77, Some("other".into()), None));
        let (url, log) = spawn_s3(Arc::clone(&objects), StubMode::default()).await;
        let mut up = uploader(&url);
        let pass = up.upload_pass(&dir, 1).await;
        assert_eq!(pass.conflicts, 1, "{pass:?}");
        assert_eq!(pass.uploaded, 1, "the next segment is still copied");
        assert_eq!(pass.pending, 1);
        assert!(!segment_has_verified_copy(&dir, &seg, 1));
        assert!(segment_has_verified_copy(&dir, &next, 10_000));
        assert!(
            !methods_for(&log, &segment_name(6)).contains(&"PUT".to_string()),
            "a conflict is never overwritten"
        );
        assert_eq!(
            objects
                .lock()
                .expect("objects")
                .get(&seg_key(6, 10_000))
                .cloned(),
            Some((77, Some("other".into()), None)),
            "the existing object is untouched"
        );
        let calls_before = methods_for(&log, &segment_name(6)).len();
        let again = up.upload_pass(&dir, 1).await;
        assert_eq!(again.conflicts, 1, "{again:?}");
        assert_eq!(again.pending, 1);
        assert_eq!(
            methods_for(&log, &segment_name(6)).len(),
            calls_before,
            "a known conflict is skipped without touching S3 again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_segment_that_grew_after_its_upload_is_copied_again_under_a_new_key() {
        let dir = tmp_dir("grew");
        let seg = plant_segment(&dir, 40, &[8u8; 5_000]);
        let objects: Objects = Arc::default();
        let (url, _log) = spawn_s3(Arc::clone(&objects), StubMode::default()).await;
        let mut up = uploader(&url);
        assert_eq!(up.upload_pass(&dir, 1).await.uploaded, 1);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&seg)
                .expect("open");
            std::io::Write::write_all(&mut f, &[9u8; 1_000]).expect("grow");
        }
        assert!(!segment_has_verified_copy(&dir, &seg, 6_000));
        let pass = up.upload_pass(&dir, 1).await;
        assert_eq!(pass.uploaded, 1, "{pass:?}");
        assert_eq!(pass.conflicts, 0);
        assert!(segment_has_verified_copy(&dir, &seg, 6_000));
        let map = objects.lock().expect("objects");
        assert!(
            map.contains_key(&seg_key(40, 5_000)),
            "the first copy stays"
        );
        assert!(map.contains_key(&seg_key(40, 6_000)));
        drop(map);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_back_that_disagrees_writes_no_marker() {
        let dir = tmp_dir("mismatch");
        let seg = plant_segment(&dir, 7, &[3u8; 20_000]);
        let objects: Objects = Arc::default();
        let mode = StubMode {
            put_len_override: Some(1),
            ..StubMode::default()
        };
        let (url, _log) = spawn_s3(Arc::clone(&objects), mode).await;
        let pass = uploader(&url).upload_pass(&dir, 10).await;
        assert_eq!(pass.uploaded + pass.already_present, 0, "{pass:?}");
        assert_eq!(pass.pending, 1);
        assert!(
            !segment_has_verified_copy(&dir, &seg, 1),
            "a read-back that disagrees writes no marker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_s3_failure_writes_no_marker_and_stops_the_pass() {
        let dir = tmp_dir("down");
        let first = plant_segment(&dir, 8, &[5u8; 1_000]);
        let second = plant_segment(&dir, 9, &[6u8; 1_000]);
        let mode = StubMode {
            head_fails: true,
            ..StubMode::default()
        };
        let (url, log) = spawn_s3(Arc::default(), mode).await;
        let pass = uploader(&url).upload_pass(&dir, 10).await;
        assert_eq!(pass.failed, 1, "{pass:?}");
        assert_eq!(
            pass.pending, 2,
            "the second segment waits for the next pass"
        );
        assert!(!segment_has_verified_copy(&dir, &first, 1));
        assert!(!segment_has_verified_copy(&dir, &second, 1));
        assert!(
            methods_for(&log, &segment_name(9)).is_empty(),
            "nothing is tried after S3 fails"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_failure_skips_the_segment_and_the_pass_moves_on() {
        let dir = tmp_dir("local");
        let first = plant_segment(&dir, 50, &[5u8; 1_000]);
        let second = plant_segment(&dir, 51, &[6u8; 1_000]);
        // A directory where the first segment's gzip scratch file goes makes
        // its compress fail locally.
        std::fs::create_dir_all(
            dir.join(RAW_UPLOAD_MARKER_SUBDIR)
                .join(TEMP_SUBDIR)
                .join(format!("{}.gz", segment_name(50))),
        )
        .expect("block scratch");
        let (url, _log) = spawn_s3(Arc::default(), StubMode::default()).await;
        let pass = uploader(&url).upload_pass(&dir, 1).await;
        assert_eq!(pass.failed, 1, "{pass:?}");
        assert_eq!(pass.uploaded, 1);
        assert!(!segment_has_verified_copy(&dir, &first, 1));
        assert!(segment_has_verified_copy(&dir, &second, 1_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_pass_tries_a_bounded_number_of_bad_segments() {
        let dir = tmp_dir("skips");
        let total = RAW_UPLOAD_MAX_SKIPS_PER_PASS + 3;
        let objects: Objects = Arc::default();
        for i in 0..total {
            let nanos = 100 + i as u128;
            plant_segment(&dir, nanos, &[1u8; 100]);
            objects
                .lock()
                .expect("objects")
                .insert(seg_key(nanos, 100), (1, Some("x".into()), None));
        }
        let (url, _log) = spawn_s3(Arc::clone(&objects), StubMode::default()).await;
        let pass = uploader(&url).upload_pass(&dir, 1).await;
        assert_eq!(pass.conflicts, RAW_UPLOAD_MAX_SKIPS_PER_PASS, "{pass:?}");
        assert_eq!(pass.pending, total);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_pass_removes_scratch_gzips_left_by_a_pass_that_died() {
        let dir = tmp_dir("scratch");
        let tmp = dir.join(RAW_UPLOAD_MARKER_SUBDIR).join(TEMP_SUBDIR);
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let stale = tmp.join("ws-frames-00000000000000000001.wal.gz");
        std::fs::write(&stale, b"half a gzip").expect("stale");
        let keep = tmp.join("notes.txt");
        std::fs::write(&keep, b"n").expect("foreign");
        let (url, _log) = spawn_s3(Arc::default(), StubMode::default()).await;
        let _pass = uploader(&url).upload_pass(&dir, 1).await;
        assert!(!stale.exists());
        assert!(keep.exists(), "only gzip scratch files are removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_pass_respects_its_per_pass_limit_oldest_first() {
        let dir = tmp_dir("limit");
        let oldest = plant_segment(&dir, 20, &[1u8; 500]);
        let newer = plant_segment(&dir, 21, &[2u8; 500]);
        let (url, _log) = spawn_s3(Arc::default(), StubMode::default()).await;
        let pass = uploader(&url).upload_pass(&dir, 1).await;
        assert_eq!(pass.uploaded, 1, "{pass:?}");
        assert_eq!(pass.pending, 1);
        assert!(segment_has_verified_copy(&dir, &oldest, 500));
        assert!(!segment_has_verified_copy(&dir, &newer, 500));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
