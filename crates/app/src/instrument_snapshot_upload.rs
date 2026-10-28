//! Daily instrument snapshot files, copied to the cold bucket and kept forever
//! (owner Quote 44, 2026-10-28).
//!
//! Owner, verbatim: *"see its just simpley betetr to go ahead with everyday
//! snapshots rigth new fiels right"*. The daily `instrument_lifecycle` write
//! stopped the same morning; from then on the point-in-time record of the
//! instrument universe is the set of dated morning files the rider already
//! writes into `data/instrument-cache`:
//!
//! | File | Writer |
//! |---|---|
//! | `dhan-nse-mapping-<date>.json` | `dhan_universe::write_mapping_atomic` |
//! | `dhan-fno-underlyings-<date>.json` | `dhan_universe::write_narrowed_spot_artifacts` |
//! | `dhan-ntm-spot-<date>.json` | `dhan_universe::write_narrowed_spot_artifacts` |
//! | `dhan-contracts-<date>.json` | `dhan_contract_universe::write_contract_artifact` (called from `dhan_universe::build_once`) |
//!
//! Each one is copied to
//! `s3://tv-<env>-cold/instrument-snapshots/<date>/<file>.gz` through the
//! same never-overwrite upload the raw-frame and spill sets use
//! (`raw_frame_upload::upload_file`): gzip, a conditional PutObject carrying
//! the gzip's SHA-256, a HeadObject that must return the same length and
//! SHA-256, and only then a durable marker in `data/instrument-cache/uploaded/`
//! recording the raw file's length, mtime and SHA-256 (the object's metadata
//! carries the raw length and SHA-256 too). An identical object already at the
//! key is reused, not re-sent; different bytes at the key go to a
//! content-addressed sidecar key, never over the first copy.
//!
//! # Where it runs
//!
//! OFF the boot critical path. The live lane waits on the mapping artifact at
//! boot; nothing here runs before that artifact exists. The rider asks for one
//! pass on its own tokio task after each successful daily build
//! ([`spawn_snapshot_upload`]). A request that finds a pass still running is
//! not doubled: it is counted (`outcome="skipped_busy"`) and the running pass
//! runs once more when it ends, so the newest files are never left for a day.
//! Each pass walks every dated file still in the folder, so a date whose copy
//! failed yesterday is retried today. Every pass is bounded by
//! [`SNAPSHOT_PASS_TIMEOUT_SECS`], so a wedged disk or a hung credential
//! lookup ends as a counted failure and frees the slot for the next request.
//!
//! # What it guards
//!
//! `dhan_universe::sweep_stale_artifacts` deletes a dated file only when its
//! marker matches the file's current length and mtime. No verified copy, no
//! delete: the file stays and is counted on [`SNAPSHOT_SWEEP_HELD_COUNTER`].
//!
//! # Honest limits
//!
//! - The S3 object is the gzip of the file (key suffix `.gz`), not the raw
//!   bytes. S3's own checksum covers the gzip; the raw SHA-256 and length are
//!   in the object's user metadata and in the local marker, and the upload
//!   checks the gzip decodes back to the raw bytes before sending.
//! - A same-day rebuild with different bytes (a restart) goes to a
//!   content-addressed sidecar key beside the first copy, never over it. The
//!   newest copy of a date is the one with the latest `raw_mtime` in its
//!   metadata, sidecars included; the sidecar upload logs a warning.
//! - Delivery is log-sink and local `/metrics` only: no alarm pages on a
//!   failed copy (a page needs a dated noise-lock row first). Nothing is lost
//!   while copies fail, because the sweep keeps every uncopied file, but the
//!   held files exist only on the box until a copy lands.
//! - A box with no cold bucket (no `TV_ENVIRONMENT` / `ENVIRONMENT` and no
//!   configured bucket) copies nothing and therefore sweeps nothing: about
//!   10 MB of local files a day grow until a bucket resolves. Counted and
//!   logged on every pass.
//! - A file rewritten after its copy (a same-day rebuild after a restart) has
//!   a new mtime, so its old marker no longer matches; the next pass copies
//!   it again (identical bytes are reused, different bytes go to a sidecar).
//! - A timed-out pass cannot stop a blocking file read already handed to the
//!   blocking pool; that thread finishes (or stays stuck) on its own.
//!
//! Cold path only: one folder listing and at most a few small files a day.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tickvault_common::error_code::ErrorCode;
use tickvault_storage::raw_frame_upload::{
    ColdObjectStore, CopyGate, SegmentOutcome, UPLOADED_SUBDIR, upload_file,
};
use tracing::{error, info, warn};

/// Key prefix inside the cold bucket. No lifecycle rule expires or deletes
/// it (the bucket-wide rule only moves objects to cheaper tiers), and the
/// Quote 35/42 wipe tool neither lists nor deletes it.
pub(crate) const INSTRUMENT_SNAPSHOT_S3_PREFIX: &str = "instrument-snapshots";

/// Suffix of the uploaded object: the upload stores the gzip of the file.
const SNAPSHOT_OBJECT_SUFFIX: &str = ".gz";

/// Upper bound on one pass, credential lookup included. A day's four files
/// take seconds; a 30-day backlog at the worst per-file S3 budget fits well
/// inside. Past it the pass is counted as failed and the slot is freed.
pub(crate) const SNAPSHOT_PASS_TIMEOUT_SECS: u64 = 1_800;

/// A pass stops early after this many files in a row ran past their S3
/// budget: the bucket is unreachable and the rest would only wait the same
/// way. They are retried on the next pass.
const SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS: usize = 3;

/// Requests in a row that found a pass still running before a warning is
/// logged (the pass may be stuck up to its timeout).
const SNAPSHOT_SKIPS_BEFORE_WARN: u32 = 2;

/// Outcomes of each snapshot copy, `outcome` = `uploaded` | `reused` |
/// `failed` | `no_bucket` | `skipped_busy` | `entry_unreadable`. Local
/// `/metrics` only: no alarm reads it.
pub(crate) const SNAPSHOT_UPLOAD_COUNTER: &str = "tv_instrument_snapshot_upload_total";

/// Old dated artifacts the sweep KEPT because no verified copy is recorded.
/// Local `/metrics` only.
pub(crate) const SNAPSHOT_SWEEP_HELD_COUNTER: &str = "tv_instrument_snapshot_sweep_held_total";

const OUTCOME_UPLOADED: &str = "uploaded";
const OUTCOME_REUSED: &str = "reused";
const OUTCOME_FAILED: &str = "failed";
const OUTCOME_NO_BUCKET: &str = "no_bucket";
const OUTCOME_SKIPPED_BUSY: &str = "skipped_busy";
const OUTCOME_ENTRY_UNREADABLE: &str = "entry_unreadable";

/// Every `outcome` label, seeded at 0 at rider start.
const ALL_OUTCOMES: [&str; 6] = [
    OUTCOME_UPLOADED,
    OUTCOME_REUSED,
    OUTCOME_FAILED,
    OUTCOME_NO_BUCKET,
    OUTCOME_SKIPPED_BUSY,
    OUTCOME_ENTRY_UNREADABLE,
];

/// One pass at a time, and a request that arrives during a pass is not lost.
struct PassGate {
    /// A pass holds the slot.
    running: AtomicBool,
    /// A request arrived while a pass ran: run once more when it ends.
    rerun: AtomicBool,
    /// Requests in a row that found the slot taken.
    consecutive_skips: AtomicU32,
}

impl PassGate {
    const fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            rerun: AtomicBool::new(false),
            consecutive_skips: AtomicU32::new(0),
        }
    }

    /// Claims the slot. `false` when a pass already holds it.
    fn try_begin(&self) -> bool {
        let claimed = self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if claimed {
            self.consecutive_skips.store(0, Ordering::Release);
        }
        claimed
    }

    /// Records a request that found the slot taken: the running pass will
    /// run once more. Returns how many requests in a row were skipped.
    fn note_skipped(&self) -> u32 {
        self.rerun.store(true, Ordering::Release);
        self.consecutive_skips
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    /// Takes the "run once more" request, if any.
    fn take_rerun(&self) -> bool {
        self.rerun.swap(false, Ordering::AcqRel)
    }

    fn end(&self) {
        self.running.store(false, Ordering::Release);
    }
}

static SNAPSHOT_PASS_GATE: PassGate = PassGate::new();

/// Releases the slot when a pass ends, however it ends (a panic included).
struct PassSlot<'gate>(&'gate PassGate);

impl Drop for PassSlot<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// Runs `one_pass` with the slot already claimed by the caller, then once
/// more for every request that arrived meanwhile. The slot is released
/// BEFORE the rerun flag is read, so a request racing the end of a pass
/// either finds the slot free (and starts its own pass) or sets the flag
/// this loop reads; it is never dropped.
async fn drive_snapshot_passes<F, Fut>(gate: &PassGate, mut one_pass: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        {
            let _slot = PassSlot(gate);
            one_pass().await;
        }
        if !gate.take_rerun() || !gate.try_begin() {
            return;
        }
    }
}

/// The date a snapshot file carries, or `None`. Only a complete `.json`
/// artifact is a snapshot; a `.json.tmp` crash leftover never is.
#[must_use]
pub(crate) fn snapshot_artifact_date(file_name: &str) -> Option<&str> {
    if !file_name.ends_with(".json") {
        return None;
    }
    crate::dhan_universe::sweepable_artifact_date(file_name)
}

/// `instrument-snapshots/<date>/<file_name>.gz`. `date` comes from
/// [`snapshot_artifact_date`] (a validated `YYYY-MM-DD`) and `file_name` from
/// the allowlisted artifact names, so the key holds no caller-chosen text.
#[must_use]
pub(crate) fn instrument_snapshot_key(date: &str, file_name: &str) -> String {
    format!("{INSTRUMENT_SNAPSHOT_S3_PREFIX}/{date}/{file_name}{SNAPSHOT_OBJECT_SUFFIX}")
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotPassSummary {
    /// Dated `.json` artifacts found in the folder.
    pub candidates: usize,
    /// Already carrying a marker that matches the file: no network call.
    pub already_copied: usize,
    /// Copied and verified this pass (canonical or sidecar key).
    pub uploaded: usize,
    /// Of `uploaded`, how many went to a content-addressed sidecar key.
    pub sidecar: usize,
    /// An identical object was already in the bucket; marked.
    pub reused: usize,
    /// Copies that failed (no marker written); retried on the next pass.
    pub failed: usize,
    /// Folder entries that could not be read (for example removed between
    /// the listing and its stat); skipped, looked at again next pass.
    pub unreadable: usize,
    /// Files not tried because the pass stopped after
    /// [`SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS`] S3 time-outs in a row.
    pub not_attempted: usize,
}

/// One dated artifact the pass looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotCandidate {
    path: PathBuf,
    file_name: String,
    date: String,
    already_copied: bool,
}

/// The folder's snapshot artifacts, plus how many entries could not be read.
#[derive(Debug, Default)]
struct SnapshotListing {
    candidates: Vec<SnapshotCandidate>,
    unreadable: usize,
}

/// Lists the snapshot artifacts in `dir`. A missing folder is an empty list
/// (first boot); a folder that cannot be opened is an error. One entry that
/// cannot be read is skipped and counted, never fatal to the rest.
fn list_snapshot_candidates(dir: &Path) -> std::io::Result<SnapshotListing> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SnapshotListing::default());
        }
        Err(err) => return Err(err),
    };
    let mut listing = SnapshotListing::default();
    for entry in entries {
        let Ok(entry) = entry else {
            listing.unreadable += 1;
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(date) = snapshot_artifact_date(name) else {
            continue;
        };
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            listing.unreadable += 1;
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let already_copied = CopyGate::Required.allows_delete(&path, &meta);
        listing.candidates.push(SnapshotCandidate {
            date: date.to_string(),
            file_name: name.to_string(),
            path,
            already_copied,
        });
    }
    // Oldest date first, so a backlog is worked from the files the sweep is
    // closest to needing.
    listing.candidates.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    Ok(listing)
}

/// Copies every dated artifact in `dir` that has no matching marker yet to
/// the cold bucket, verified. Never panics; every failure is counted on
/// [`SNAPSHOT_UPLOAD_COUNTER`] and logged with a coded STORAGE-GAP-04 error.
pub(crate) async fn run_snapshot_pass<S: ColdObjectStore>(
    store: &S,
    dir: &Path,
) -> SnapshotPassSummary {
    let mut summary = SnapshotPassSummary::default();
    let listing_dir = dir.to_path_buf();
    let listed = tokio::task::spawn_blocking(move || list_snapshot_candidates(&listing_dir)).await;
    let listing = match listed {
        Ok(Ok(listing)) => listing,
        Ok(Err(err)) => {
            record_failure(&mut summary, "folder", &format!("listing failed: {err}"));
            return summary;
        }
        Err(err) => {
            record_failure(
                &mut summary,
                "folder",
                &format!("listing task failed: {err}"),
            );
            return summary;
        }
    };
    if listing.unreadable > 0 {
        summary.unreadable = listing.unreadable;
        metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_ENTRY_UNREADABLE)
            .increment(listing.unreadable as u64);
        info!(
            unreadable = listing.unreadable,
            "instrument snapshot folder entries could not be read this pass; \
             they are looked at again on the next pass"
        );
    }
    let markers_dir = dir.join(UPLOADED_SUBDIR);
    let mut consecutive_timeouts = 0_usize;
    let total = listing.candidates.len();
    for (index, candidate) in listing.candidates.into_iter().enumerate() {
        if consecutive_timeouts >= SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS {
            summary.not_attempted = total - index;
            record_failure(
                &mut summary,
                "pass",
                &format!(
                    "{consecutive_timeouts} files in a row ran past their S3 budget; \
                     the bucket looks unreachable, {} files left for the next pass",
                    total - index
                ),
            );
            break;
        }
        summary.candidates += 1;
        if candidate.already_copied {
            summary.already_copied += 1;
            continue;
        }
        let key = instrument_snapshot_key(&candidate.date, &candidate.file_name);
        match upload_file(store, &markers_dir, &candidate.path, &key).await {
            SegmentOutcome::Uploaded { key } => {
                consecutive_timeouts = 0;
                summary.uploaded += 1;
                metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_UPLOADED)
                    .increment(1);
                info!(
                    file = %candidate.file_name,
                    bucket = store.bucket_name(),
                    key = %key,
                    "instrument snapshot copied to the cold bucket and verified"
                );
            }
            SegmentOutcome::UploadedSidecar { key } => {
                consecutive_timeouts = 0;
                summary.uploaded += 1;
                summary.sidecar += 1;
                metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_UPLOADED)
                    .increment(1);
                warn!(
                    file = %candidate.file_name,
                    bucket = store.bucket_name(),
                    key = %key,
                    "instrument snapshot for this date already had DIFFERENT bytes in \
                     the cold bucket (a same-day rebuild); this copy is kept beside it \
                     at a content-addressed key. The newest copy of the date is the one \
                     whose raw-mtime is latest"
                );
            }
            SegmentOutcome::Reused { .. } => {
                consecutive_timeouts = 0;
                summary.reused += 1;
                metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_REUSED)
                    .increment(1);
            }
            SegmentOutcome::Failed { reason } => {
                consecutive_timeouts = 0;
                record_failure(&mut summary, &candidate.file_name, &reason);
            }
            SegmentOutcome::TimedOut { reason } => {
                consecutive_timeouts += 1;
                record_failure(&mut summary, &candidate.file_name, &reason);
            }
        }
    }
    summary
}

/// [`run_snapshot_pass`] under a time bound. `None` (counted as a failed
/// pass) when the bound passed first; the files already marked stay marked.
async fn run_bounded_snapshot_pass<S: ColdObjectStore>(
    store: &S,
    dir: &Path,
    limit: Duration,
) -> Option<SnapshotPassSummary> {
    if let Ok(summary) = tokio::time::timeout(limit, run_snapshot_pass(store, dir)).await {
        Some(summary)
    } else {
        let mut ignored = SnapshotPassSummary::default();
        record_failure(
            &mut ignored,
            "pass",
            &format!("the pass did not finish within {limit:?}; retried on the next pass"),
        );
        None
    }
}

/// Counts and logs one failed copy (or a failed listing, `what = "folder"`,
/// or a stopped pass, `what = "pass"`).
fn record_failure(summary: &mut SnapshotPassSummary, what: &str, reason: &str) {
    summary.failed += 1;
    metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_FAILED).increment(1);
    error!(
        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
        source = "instrument_snapshot_upload",
        file = what,
        reason,
        "instrument snapshot NOT copied to the cold bucket; the local file is \
         kept (the sweep deletes nothing without a verified copy) and the copy \
         is retried on the next daily pass"
    );
}

/// Seeds the counters at 0 so a scrape shows them before the first pass.
/// Called once at rider start, not right before the copy.
pub(crate) fn register_snapshot_metrics() {
    for outcome in ALL_OUTCOMES {
        metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => outcome).increment(0);
    }
    metrics::counter!(SNAPSHOT_SWEEP_HELD_COUNTER).increment(0);
}

/// Claims the pass slot for a new request. `false` when a pass is running:
/// the request is then counted as skipped and the running pass runs once
/// more when it ends.
fn claim_or_queue_pass(gate: &PassGate) -> bool {
    if gate.try_begin() {
        return true;
    }
    metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_SKIPPED_BUSY).increment(1);
    let skips = gate.note_skipped();
    if skips >= SNAPSHOT_SKIPS_BEFORE_WARN {
        warn!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            source = "instrument_snapshot_pass_busy",
            requests_in_a_row = skips,
            "instrument snapshot pass still running across several requests; it is \
             bounded and runs once more when it ends, but no new copy lands until then"
        );
    } else {
        info!("instrument snapshot pass still running; it runs once more when it ends");
    }
    false
}

/// One request's work: resolve the bucket, then one bounded pass.
async fn one_snapshot_request(bucket: &str) {
    let limit = Duration::from_secs(SNAPSHOT_PASS_TIMEOUT_SECS);
    let loaded =
        tokio::time::timeout(limit, tickvault_storage::s3_cold::S3Cold::load(bucket)).await;
    let store = match loaded {
        Ok(Some(store)) => store,
        Ok(None) => {
            metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_NO_BUCKET).increment(1);
            error!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                source = "instrument_snapshot_no_bucket",
                "instrument snapshots NOT copied: no cold bucket resolves for this \
                 environment (no configured bucket, TV_ENVIRONMENT / ENVIRONMENT unset). \
                 The local files are kept; the sweep deletes none of them until a copy \
                 is verified"
            );
            return;
        }
        Err(_elapsed) => {
            let mut ignored = SnapshotPassSummary::default();
            record_failure(
                &mut ignored,
                "pass",
                "the cold-bucket client did not load in time; retried on the next pass",
            );
            return;
        }
    };
    let dir = Path::new(crate::dhan_universe::MAPPING_DIR);
    if let Some(summary) = run_bounded_snapshot_pass(&store, dir, limit).await {
        info!(
            bucket = store.bucket(),
            candidates = summary.candidates,
            already_copied = summary.already_copied,
            uploaded = summary.uploaded,
            sidecar = summary.sidecar,
            reused = summary.reused,
            failed = summary.failed,
            unreadable = summary.unreadable,
            not_attempted = summary.not_attempted,
            "instrument snapshot pass finished"
        );
    }
}

/// Starts one snapshot pass over `data/instrument-cache` on its own task,
/// copying to `bucket` (the configured cold bucket; empty means
/// `tv-<env>-cold` from the runtime environment, as for the raw frames).
/// Returns `None` (and starts nothing) while an earlier pass is still
/// running; that pass then runs once more when it ends.
// TEST-EXEMPT: loads the live AWS config chain and the real cold bucket; the
// pass it drives is tested through `run_snapshot_pass` /
// `run_bounded_snapshot_pass` with an in-memory store, the slot and rerun
// through `claim_or_queue_pass` / `drive_snapshot_passes` on a test gate.
pub(crate) fn spawn_snapshot_upload(bucket: String) -> Option<tokio::task::JoinHandle<()>> {
    if !claim_or_queue_pass(&SNAPSHOT_PASS_GATE) {
        return None;
    }
    Some(tokio::spawn(async move {
        drive_snapshot_passes(&SNAPSHOT_PASS_GATE, || one_snapshot_request(&bucket)).await;
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use tickvault_storage::raw_frame_upload::CopyGate;
    use tickvault_storage::s3_cold::S3ObjectMeta;

    /// In-memory bucket: key -> (length, base64 SHA-256). Counts calls so a
    /// test can prove a marked file costs no network call.
    #[derive(Default)]
    struct FakeBucket {
        objects: Mutex<HashMap<String, (u64, String)>>,
        puts: AtomicUsize,
        heads: AtomicUsize,
        fail_puts: bool,
        /// Every HeadObject waits forever (a hung connection).
        hang: bool,
        /// Per-file S3 budget override (the production budget otherwise).
        budget: Option<std::time::Duration>,
    }

    impl ColdObjectStore for FakeBucket {
        fn bucket_name(&self) -> &str {
            "tv-test-cold"
        }

        fn call_budget(&self, _gzip_len: u64) -> std::time::Duration {
            // The production floor is 60 s; tests that do not set a budget
            // never reach it.
            self.budget
                .unwrap_or_else(|| std::time::Duration::from_secs(60))
        }

        async fn head_object(&self, key: &str) -> anyhow::Result<Option<S3ObjectMeta>> {
            self.heads.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                std::future::pending::<()>().await;
            }
            let objects = self
                .objects
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            Ok(objects.get(key).map(|(len, sha)| S3ObjectMeta {
                len: *len,
                checksum_sha256_b64: Some(sha.clone()),
            }))
        }

        async fn put_object_if_absent(
            &self,
            key: &str,
            body: Vec<u8>,
            sha256_b64: &str,
            _metadata: &[(&'static str, String)],
        ) -> anyhow::Result<()> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            if self.fail_puts {
                anyhow::bail!("injected PutObject failure");
            }
            let mut objects = self
                .objects
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            if objects.contains_key(key) {
                anyhow::bail!("412 precondition failed");
            }
            objects.insert(key.to_string(), (body.len() as u64, sha256_b64.to_string()));
            Ok(())
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "tv_instrument_snapshot_{tag}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_snapshot_artifact_date_takes_only_complete_dated_json() {
        assert_eq!(
            snapshot_artifact_date("dhan-contracts-2026-10-20.json"),
            Some("2026-10-20")
        );
        assert_eq!(
            snapshot_artifact_date("dhan-nse-mapping-2026-10-20.json"),
            Some("2026-10-20")
        );
        assert_eq!(
            snapshot_artifact_date("dhan-fno-underlyings-2026-10-20.json"),
            Some("2026-10-20")
        );
        assert_eq!(
            snapshot_artifact_date("dhan-ntm-spot-2026-10-20.json"),
            Some("2026-10-20")
        );
        // A crash leftover is never a snapshot.
        assert_eq!(
            snapshot_artifact_date("dhan-contracts-2026-10-20.json.tmp"),
            None
        );
        // Unknown names and malformed dates are never copied.
        for bad in [
            "plan-2026-10-20.json",
            "dhan-contracts-2026-10-2a.json",
            "dhan-contracts.json",
            "uploaded",
        ] {
            assert_eq!(snapshot_artifact_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn test_instrument_snapshot_key_is_dated_and_under_the_prefix() {
        assert_eq!(
            instrument_snapshot_key("2026-10-28", "dhan-contracts-2026-10-28.json"),
            "instrument-snapshots/2026-10-28/dhan-contracts-2026-10-28.json.gz"
        );
        assert_eq!(INSTRUMENT_SNAPSHOT_S3_PREFIX, "instrument-snapshots");
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_copies_verifies_and_marks_then_skips_without_network() {
        let dir = scratch_dir("pass");
        for name in [
            "dhan-contracts-2026-10-20.json",
            "dhan-nse-mapping-2026-10-21.json",
        ] {
            std::fs::write(dir.join(name), format!("{{\"file\":\"{name}\"}}")).unwrap();
        }
        std::fs::write(dir.join("dhan-contracts-2026-10-21.json.tmp"), b"partial").unwrap();
        std::fs::write(dir.join("notes.txt"), b"not an artifact").unwrap();
        let bucket = FakeBucket::default();

        let first = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(first.candidates, 2, "{first:?}");
        assert_eq!(first.uploaded, 2, "{first:?}");
        assert_eq!(first.failed, 0, "{first:?}");
        {
            let objects = bucket.objects.lock().unwrap();
            assert!(
                objects.contains_key(
                    "instrument-snapshots/2026-10-20/dhan-contracts-2026-10-20.json.gz"
                )
            );
            assert!(objects.contains_key(
                "instrument-snapshots/2026-10-21/dhan-nse-mapping-2026-10-21.json.gz"
            ));
            assert_eq!(objects.len(), 2, "no .tmp or foreign file is copied");
        }
        // Each copied file now passes the sweep's copy gate.
        for name in [
            "dhan-contracts-2026-10-20.json",
            "dhan-nse-mapping-2026-10-21.json",
        ] {
            let path = dir.join(name);
            let meta = std::fs::metadata(&path).unwrap();
            assert!(CopyGate::Required.allows_delete(&path, &meta), "{name}");
        }

        // Second pass: idempotent, and a marked file costs no network call.
        let puts = bucket.puts.load(Ordering::SeqCst);
        let heads = bucket.heads.load(Ordering::SeqCst);
        let second = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(second.already_copied, 2, "{second:?}");
        assert_eq!(second.uploaded + second.reused + second.failed, 0);
        assert_eq!(bucket.puts.load(Ordering::SeqCst), puts);
        assert_eq!(bucket.heads.load(Ordering::SeqCst), heads);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_reuses_an_identical_object_without_reuploading() {
        let dir = scratch_dir("reuse");
        let name = "dhan-ntm-spot-2026-10-22.json";
        std::fs::write(dir.join(name), b"{\"spots\":[1,2,3]}").unwrap();
        let bucket = FakeBucket::default();
        assert_eq!(run_snapshot_pass(&bucket, &dir).await.uploaded, 1);
        // The local marker is lost (a rebuilt box): the object is still there.
        std::fs::remove_dir_all(dir.join(UPLOADED_SUBDIR)).unwrap();
        let puts = bucket.puts.load(Ordering::SeqCst);
        let again = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(again.reused, 1, "{again:?}");
        assert_eq!(
            bucket.puts.load(Ordering::SeqCst),
            puts,
            "an identical object is reused, never sent again"
        );
        let meta = std::fs::metadata(dir.join(name)).unwrap();
        assert!(CopyGate::Required.allows_delete(&dir.join(name), &meta));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_counts_a_failed_copy_and_writes_no_marker() {
        let dir = scratch_dir("fail");
        let name = "dhan-contracts-2026-10-23.json";
        std::fs::write(dir.join(name), b"{}").unwrap();
        let bucket = FakeBucket {
            fail_puts: true,
            ..FakeBucket::default()
        };
        let summary = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(summary.failed, 1, "{summary:?}");
        assert_eq!(summary.uploaded, 0);
        let meta = std::fs::metadata(dir.join(name)).unwrap();
        assert!(
            !CopyGate::Required.allows_delete(&dir.join(name), &meta),
            "a failed copy must leave the file undeletable"
        );
        // Retried on the next pass once the bucket works.
        let healthy = FakeBucket::default();
        assert_eq!(run_snapshot_pass(&healthy, &dir).await.uploaded, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_on_a_missing_folder_does_nothing() {
        let dir = std::env::temp_dir().join("tv_instrument_snapshot_never_created_folder");
        let bucket = FakeBucket::default();
        let summary = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(summary, SnapshotPassSummary::default());
        assert_eq!(bucket.heads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_list_snapshot_candidates_orders_oldest_date_first() {
        let dir = scratch_dir("order");
        for name in [
            "dhan-contracts-2026-10-22.json",
            "dhan-contracts-2026-10-20.json",
            "dhan-nse-mapping-2026-10-21.json",
        ] {
            std::fs::write(dir.join(name), b"{}").unwrap();
        }
        let listing = list_snapshot_candidates(&dir).unwrap();
        assert_eq!(listing.unreadable, 0);
        let dates: Vec<String> = listing.candidates.into_iter().map(|c| c.date).collect();
        assert_eq!(dates, ["2026-10-20", "2026-10-21", "2026-10-22"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A dated name that is not a regular file (here a folder) is skipped,
    /// never copied and never a failure of the whole listing.
    #[test]
    fn test_list_snapshot_candidates_skips_a_non_file_entry_and_keeps_the_rest() {
        let dir = scratch_dir("nonfile");
        std::fs::create_dir_all(dir.join("dhan-contracts-2026-10-19.json")).unwrap();
        std::fs::write(dir.join("dhan-contracts-2026-10-20.json"), b"{}").unwrap();
        let listing = list_snapshot_candidates(&dir).unwrap();
        let names: Vec<String> = listing
            .candidates
            .into_iter()
            .map(|c| c.file_name)
            .collect();
        assert_eq!(names, ["dhan-contracts-2026-10-20.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_claim_or_queue_pass_queues_a_rerun_and_counts_the_skip() {
        // A private gate, so no other test can race the process-wide one.
        let gate = PassGate::new();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);
        assert!(claim_or_queue_pass(&gate), "a free slot is claimed");
        assert!(
            !claim_or_queue_pass(&gate),
            "a request while a pass runs must not start a second pass"
        );
        assert!(!claim_or_queue_pass(&gate));
        assert_eq!(gate.consecutive_skips.load(Ordering::Acquire), 2);
        assert!(
            gate.take_rerun(),
            "the skipped request is queued as a rerun"
        );
        assert!(!gate.take_rerun(), "the rerun is taken once");
        let rendered = handle.render();
        assert!(
            rendered.contains(&format!(
                "{SNAPSHOT_UPLOAD_COUNTER}{{outcome=\"{OUTCOME_SKIPPED_BUSY}\"}} 2"
            )),
            "{rendered}"
        );
        gate.end();
        assert!(
            claim_or_queue_pass(&gate),
            "the slot is free once the pass ends"
        );
        assert_eq!(
            gate.consecutive_skips.load(Ordering::Acquire),
            0,
            "a started pass resets the skip run"
        );
        gate.end();
    }

    #[test]
    fn test_spawn_snapshot_upload_starts_nothing_while_a_pass_runs() {
        // The only test touching the process-wide gate.
        assert!(SNAPSHOT_PASS_GATE.try_begin(), "a free slot is claimed");
        assert!(
            spawn_snapshot_upload(String::new()).is_none(),
            "a request while a pass runs returns before any task is spawned"
        );
        assert!(SNAPSHOT_PASS_GATE.take_rerun(), "and is queued as a rerun");
        SNAPSHOT_PASS_GATE.end();
    }

    #[tokio::test]
    async fn test_drive_snapshot_passes_runs_once_more_for_a_request_during_a_pass() {
        let gate = PassGate::new();
        assert!(gate.try_begin());
        let runs = AtomicUsize::new(0);
        drive_snapshot_passes(&gate, || {
            let run = runs.fetch_add(1, Ordering::SeqCst);
            if run == 0 {
                // A request arrives while the first pass runs.
                assert!(!claim_or_queue_pass(&gate));
            }
            async {}
        })
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the queued request ran");
        assert!(
            !gate.running.load(Ordering::Acquire),
            "the slot is released when the last pass ends"
        );
        // Without a request during the pass, exactly one pass runs.
        assert!(gate.try_begin());
        let single = AtomicUsize::new(0);
        drive_snapshot_passes(&gate, || {
            single.fetch_add(1, Ordering::SeqCst);
            async {}
        })
        .await;
        assert_eq!(single.load(Ordering::SeqCst), 1);
        assert!(!gate.running.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn test_run_bounded_snapshot_pass_ends_a_hung_pass_as_a_counted_failure() {
        let dir = scratch_dir("hung");
        let name = "dhan-contracts-2026-10-25.json";
        std::fs::write(dir.join(name), b"{}").unwrap();
        let bucket = FakeBucket {
            hang: true,
            ..FakeBucket::default()
        };
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let finished = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            run_bounded_snapshot_pass(&bucket, &dir, Duration::from_millis(200)).await
        };
        assert!(finished.is_none(), "a hung pass must end at its bound");
        assert!(
            handle.render().contains(&format!(
                "{SNAPSHOT_UPLOAD_COUNTER}{{outcome=\"{OUTCOME_FAILED}\"}} 1"
            )),
            "the timed-out pass is counted"
        );
        let meta = std::fs::metadata(dir.join(name)).unwrap();
        assert!(!CopyGate::Required.allows_delete(&dir.join(name), &meta));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_stops_after_three_s3_timeouts_in_a_row() {
        let dir = scratch_dir("unreachable");
        for day in 20..25 {
            std::fs::write(
                dir.join(format!("dhan-contracts-2026-10-{day}.json")),
                b"{}",
            )
            .unwrap();
        }
        let bucket = FakeBucket {
            hang: true,
            budget: Some(Duration::from_millis(20)),
            ..FakeBucket::default()
        };
        let summary = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(summary.candidates, SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS);
        assert_eq!(summary.not_attempted, 5 - SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS);
        // One failure per timed-out file plus one for the stopped pass.
        assert_eq!(summary.failed, SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS + 1);
        assert_eq!(
            bucket.heads.load(Ordering::SeqCst),
            SNAPSHOT_MAX_CONSECUTIVE_TIMEOUTS,
            "no S3 call after the pass stopped"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_register_snapshot_metrics_seeds_every_outcome_at_zero() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, register_snapshot_metrics);
        let rendered = handle.render();
        assert_eq!(ALL_OUTCOMES.len(), 6);
        for outcome in ALL_OUTCOMES {
            let line = format!("{SNAPSHOT_UPLOAD_COUNTER}{{outcome=\"{outcome}\"}} 0");
            assert!(rendered.contains(&line), "missing `{line}` in:\n{rendered}");
        }
        assert!(
            rendered.contains(&format!("{SNAPSHOT_SWEEP_HELD_COUNTER} 0")),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_counts_each_outcome_on_the_counter() {
        let dir = scratch_dir("counter");
        std::fs::write(dir.join("dhan-contracts-2026-10-24.json"), b"{\"a\":1}").unwrap();
        std::fs::write(dir.join("dhan-contracts-2026-10-25.json"), b"{\"b\":2}").unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // uploaded: two fresh files.
        let bucket = FakeBucket::default();
        assert_eq!(run_snapshot_pass(&bucket, &dir).await.uploaded, 2);
        // reused: the markers are lost, the objects are still there.
        std::fs::remove_dir_all(dir.join(UPLOADED_SUBDIR)).unwrap();
        assert_eq!(run_snapshot_pass(&bucket, &dir).await.reused, 2);
        // failed: a third file and a bucket that refuses every put.
        std::fs::write(dir.join("dhan-contracts-2026-10-26.json"), b"{}").unwrap();
        let failing = FakeBucket {
            fail_puts: true,
            ..FakeBucket::default()
        };
        assert_eq!(run_snapshot_pass(&failing, &dir).await.failed, 1);

        let rendered = handle.render();
        for (outcome, count) in [
            (OUTCOME_UPLOADED, 2),
            (OUTCOME_REUSED, 2),
            (OUTCOME_FAILED, 1),
        ] {
            let line = format!("{SNAPSHOT_UPLOAD_COUNTER}{{outcome=\"{outcome}\"}} {count}");
            assert!(rendered.contains(&line), "missing `{line}` in:\n{rendered}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_sends_different_bytes_to_a_sidecar_never_over_the_first() {
        let dir = scratch_dir("sidecar");
        let name = "dhan-contracts-2026-10-27.json";
        std::fs::write(dir.join(name), b"{\"rebuilt\":true}").unwrap();
        let canonical = instrument_snapshot_key("2026-10-27", name);
        let bucket = FakeBucket::default();
        bucket
            .objects
            .lock()
            .unwrap()
            .insert(canonical.clone(), (3, "AAAA".to_string()));
        let summary = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(summary.uploaded, 1, "{summary:?}");
        assert_eq!(summary.sidecar, 1, "{summary:?}");
        let objects = bucket.objects.lock().unwrap();
        assert_eq!(
            objects.get(&canonical),
            Some(&(3, "AAAA".to_string())),
            "the first copy is never overwritten"
        );
        assert_eq!(objects.len(), 2, "the new bytes sit beside it");
        drop(objects);
        let meta = std::fs::metadata(dir.join(name)).unwrap();
        assert!(CopyGate::Required.allows_delete(&dir.join(name), &meta));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
