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
//! A marker can outlive its copy (a bucket-wide delete such as owner
//! Quote 46). So, since the 2026-10-28 review, the sweep runs only here,
//! after a pass that re-checked every marked file's copy with one HeadObject
//! (the key recorded in its marker): a copy the bucket no longer holds drops
//! the marker (`outcome="copy_gone"`) and the file is copied again in the same
//! pass; a re-check the bucket does not answer (`outcome="recheck_failed"`)
//! keeps the file and that pass sweeps nothing
//! ([`SnapshotPassSummary::sweep_allowed`]); so does a folder entry the
//! listing could not read (`outcome="entry_unreadable"`), because it was
//! never re-checked. Cost: one HeadObject per marked file per pass, about 30
//! a day for the 7-day window.
//!
//! The marker is never trusted on its own (2026-10-28 review). The sweep
//! deletes a file only when this pass proved its copy itself: the re-check
//! HeadObject matched, or the pass uploaded (or reused an identical object)
//! and verified it ([`SnapshotPassSummary::proven`]). A marker that outlives
//! its copy and whose unlink then fails (`stale_marker_kept`, logged) still
//! matches the file; if the re-upload also fails or times out, the file is
//! not in `proven` and the sweep keeps it.
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
//! - A pass that panics (or whose task is dropped at shutdown) frees the slot
//!   and drops a rerun queued on it; the next request runs a pass. Every
//!   other request is served: claiming the slot and queueing a rerun is one
//!   atomic step, and so is ending a pass (fixed 2026-10-28, review).
//! - The re-check (or the verified upload) proves the object is at its key
//!   with the recorded gzip length when it runs; the sweep runs only after
//!   the whole pass. A delete in the bucket between a file's proof and the
//!   sweep is not caught. That gap is NOT seconds: it is bounded by the pass
//!   bound, [`SNAPSHOT_PASS_TIMEOUT_SECS`] (1,800 s, 30 minutes; a pass that
//!   runs past it sweeps nothing), because the first file's proof can be
//!   followed by every other file's S3 calls, plus the sweep's own folder
//!   walk, which has no time bound of its own (a wedged disk holds it). A box
//!   with no cold bucket sweeps nothing at all (before, the sweep ran in the
//!   build and trusted the markers alone).
//!
//! Cold path only: one folder listing and at most a few small files a day.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
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
/// `failed` | `no_bucket` | `skipped_busy` | `entry_unreadable` |
/// `copy_gone` | `recheck_failed`. Local `/metrics` only: no alarm reads it.
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
/// A file's marker named a copy the bucket no longer holds (for example
/// after a bucket-wide delete): the marker is dropped and the file copied
/// again.
const OUTCOME_COPY_GONE: &str = "copy_gone";
/// The re-check of a marked file's copy could not reach the bucket: the
/// file is kept and this pass sweeps nothing.
const OUTCOME_RECHECK_FAILED: &str = "recheck_failed";

/// Every `outcome` label, seeded at 0 at rider start.
const ALL_OUTCOMES: [&str; 8] = [
    OUTCOME_UPLOADED,
    OUTCOME_REUSED,
    OUTCOME_FAILED,
    OUTCOME_NO_BUCKET,
    OUTCOME_SKIPPED_BUSY,
    OUTCOME_ENTRY_UNREADABLE,
    OUTCOME_COPY_GONE,
    OUTCOME_RECHECK_FAILED,
];

/// No pass is running.
const GATE_IDLE: u8 = 0;
/// A pass holds the slot and no request arrived during it.
const GATE_RUNNING: u8 = 1;
/// A pass holds the slot and at least one request arrived during it: the
/// pass runs once more when it ends.
const GATE_RUNNING_RERUN: u8 = 2;

/// One pass at a time, and a request that arrives during a pass is not lost.
///
/// The whole gate is ONE atomic state (`GATE_*`), so "is a pass running?"
/// and "queue a rerun" are a single compare-and-swap for a request, and
/// "was a rerun queued? then keep the slot, else free it" is a single
/// compare-and-swap for the pass that ends. Before 2026-10-28 these were two
/// flags: a request could find the slot taken, the pass could then end and
/// read no rerun, and only then would the request set its flag, so no pass
/// ran for it until the next day's request
/// (`test_request_pass_never_drops_a_request_racing_the_end_of_a_pass`).
/// Lock-free, no allocation; cold path (a few requests a day).
struct PassGate {
    /// `GATE_IDLE`, `GATE_RUNNING` or `GATE_RUNNING_RERUN`.
    state: AtomicU8,
    /// Requests in a row that found the slot taken.
    consecutive_skips: AtomicU32,
}

/// What a request got from [`PassGate::request_pass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassRequest {
    /// The slot was free and is now held by the caller, who must run a pass.
    Claimed,
    /// A pass holds the slot; it runs once more when it ends. Carries how
    /// many requests in a row found the slot taken.
    Queued(u32),
}

impl PassGate {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(GATE_IDLE),
            consecutive_skips: AtomicU32::new(0),
        }
    }

    /// Claims the slot, or queues a rerun on the pass holding it, in one
    /// atomic step: a request is never lost between the two.
    fn request_pass(&self) -> PassRequest {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let next = if current == GATE_IDLE {
                GATE_RUNNING
            } else {
                GATE_RUNNING_RERUN
            };
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) if current == GATE_IDLE => {
                    self.consecutive_skips.store(0, Ordering::Release);
                    return PassRequest::Claimed;
                }
                Ok(_) => {
                    let skips = self
                        .consecutive_skips
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    return PassRequest::Queued(skips);
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Ends the pass holding the slot. `true` when a request arrived during
    /// it: the slot stays held and the caller runs once more. `false` frees
    /// the slot. One atomic step, so a request racing this either was queued
    /// before it (and is run) or finds the slot free (and claims it).
    fn finish_pass(&self) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let (next, again) = if current == GATE_RUNNING_RERUN {
                (GATE_RUNNING, true)
            } else {
                (GATE_IDLE, false)
            };
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if again {
                        self.consecutive_skips.store(0, Ordering::Release);
                    }
                    return again;
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Frees the slot unconditionally (a pass that panicked or whose task
    /// was dropped). A rerun queued on that pass is dropped with it; the
    /// next request claims the free slot.
    fn release(&self) {
        self.state.store(GATE_IDLE, Ordering::Release);
    }

    /// Whether a pass holds the slot (tests only).
    #[cfg(test)]
    fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) != GATE_IDLE
    }
}

static SNAPSHOT_PASS_GATE: PassGate = PassGate::new();

/// Holds the slot for one pass. Frees it if the pass ends without
/// [`PassSlot::finish`] (a panic, or the task dropped at shutdown).
struct PassSlot<'gate> {
    gate: &'gate PassGate,
    armed: bool,
}

impl<'gate> PassSlot<'gate> {
    fn new(gate: &'gate PassGate) -> Self {
        Self { gate, armed: true }
    }

    /// Ends the pass normally; see [`PassGate::finish_pass`].
    fn finish(mut self) -> bool {
        self.armed = false;
        self.gate.finish_pass()
    }
}

impl Drop for PassSlot<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.gate.release();
        }
    }
}

/// Runs `one_pass` with the slot already claimed by the caller, then once
/// more for every request that arrived meanwhile (several requests during
/// one pass are one rerun: the rerun lists the folder after all of them).
async fn drive_snapshot_passes<F, Fut>(gate: &PassGate, mut one_pass: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        let slot = PassSlot::new(gate);
        one_pass().await;
        if !slot.finish() {
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
    /// The folder was listed (a failed listing sweeps nothing).
    pub listed: bool,
    /// Dated `.json` artifacts found in the folder.
    pub candidates: usize,
    /// Already carrying a marker that matches the file, and the copy the
    /// marker names is still in the bucket (one HeadObject, no upload).
    pub already_copied: usize,
    /// Marked files whose copy the bucket no longer holds (a bucket-wide
    /// delete): the marker is dropped and the file is copied again this pass.
    pub copy_gone: usize,
    /// Marked files whose copy could not be re-checked (the bucket did not
    /// answer): kept, and this pass sweeps nothing.
    pub unconfirmed: usize,
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
    /// Of `copy_gone`, the files whose stale marker could not be removed
    /// (the unlink failed, or its task did not finish). The marker still
    /// matches the file; the sweep keeps the file anyway unless this pass
    /// re-uploads it (it is not in `proven`).
    pub stale_marker_kept: usize,
    /// File names whose S3 copy THIS pass confirmed itself: a HeadObject
    /// that matched the marker's key and length, or an upload (canonical,
    /// sidecar or reused identical object) it verified. The sweep deletes
    /// only names in this set, whatever the markers on disk say. At most
    /// one entry per dated file in the folder (about 30), cold.
    pub proven: std::collections::BTreeSet<String>,
}

impl SnapshotPassSummary {
    /// Whether the retention sweep may run after this pass: the folder was
    /// listed, every entry in it was read (an entry the listing could not
    /// read was never re-checked, 2026-10-28 review), every file was looked
    /// at, and every marker the sweep could rely on was re-checked against
    /// the bucket. Otherwise a marker left from a copy someone deleted could
    /// let the sweep delete the only remaining file. Even when it is `true`,
    /// the sweep deletes only the files in [`Self::proven`].
    #[must_use]
    pub(crate) fn sweep_allowed(&self) -> bool {
        self.listed && self.unreadable == 0 && self.not_attempted == 0 && self.unconfirmed == 0
    }
}

/// What the bucket says about the copy a marked file's marker names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CopyRecheck {
    /// The object is there with the recorded gzip length.
    Present,
    /// Nothing at the key, or a different object: the copy is gone.
    Gone,
    /// The marker vanished since the listing: treat the file as unmarked.
    NoMarker,
    /// The bucket did not answer (error or time-out); `timed_out` feeds the
    /// pass's run of time-outs.
    Unconfirmed { reason: String, timed_out: bool },
}

/// Re-checks, with one HeadObject under the store's call budget, that the
/// copy recorded in `path`'s marker is still in the bucket. The marker can
/// outlive its copy (a bucket-wide delete such as owner Quote 46); a stale
/// marker would let the sweep delete the only remaining file. Cold path.
async fn recheck_marked_copy<S: ColdObjectStore>(store: &S, path: &Path) -> CopyRecheck {
    let marker_path = path.to_path_buf();
    let marker = match tokio::task::spawn_blocking(move || {
        tickvault_storage::raw_frame_upload::read_sibling_marker(&marker_path)
    })
    .await
    {
        Ok(Some(marker)) => marker,
        Ok(None) => return CopyRecheck::NoMarker,
        Err(err) => {
            return CopyRecheck::Unconfirmed {
                reason: format!("marker read task failed: {err}"),
                timed_out: false,
            };
        }
    };
    if marker.bucket != store.bucket_name() {
        // Copied to another bucket: nothing proves a copy in this one.
        return CopyRecheck::Gone;
    }
    let budget = store.call_budget(marker.gzip_len);
    match tokio::time::timeout(budget, store.head_object(&marker.key)).await {
        Ok(Ok(Some(meta))) if meta.len == marker.gzip_len => CopyRecheck::Present,
        Ok(Ok(_)) => CopyRecheck::Gone,
        Ok(Err(err)) => CopyRecheck::Unconfirmed {
            reason: format!("HeadObject {} failed: {err}", marker.key),
            timed_out: false,
        },
        Err(_elapsed) => CopyRecheck::Unconfirmed {
            reason: format!("HeadObject {} ran past {budget:?}", marker.key),
            timed_out: true,
        },
    }
}

/// Removes the marker of `path`, whose copy the bucket no longer holds, and
/// reports whether it is gone afterwards: `false` when the unlink failed
/// (the marker file is still there) or its blocking task did not finish.
/// The caller counts a `false`; the sweep never trusts the marker either
/// way, because it deletes only the files this pass proved
/// ([`SnapshotPassSummary::proven`]). Cold path: one unlink and one stat on
/// the blocking pool.
async fn drop_stale_marker(path: &Path) -> bool {
    let gone = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        CopyGate::Required.forget(&gone);
        marker_file_absent(&gone)
    })
    .await
    .unwrap_or(false)
}

/// Whether no marker file exists for `path` in its `uploaded/` folder. A
/// stat that fails for any reason other than not-found counts as present.
fn marker_file_absent(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    path.parent().is_some_and(|dir| {
        matches!(
            std::fs::symlink_metadata(dir.join(UPLOADED_SUBDIR).join(name)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound
        )
    })
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
    summary.listed = true;
    if listing.unreadable > 0 {
        summary.unreadable = listing.unreadable;
        metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_ENTRY_UNREADABLE)
            .increment(listing.unreadable as u64);
        info!(
            unreadable = listing.unreadable,
            "instrument snapshot folder entries could not be read this pass; \
             this pass sweeps nothing and they are looked at again on the next pass"
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
            match recheck_marked_copy(store, &candidate.path).await {
                CopyRecheck::Present => {
                    consecutive_timeouts = 0;
                    summary.already_copied += 1;
                    summary.proven.insert(candidate.file_name);
                    continue;
                }
                CopyRecheck::Gone => {
                    consecutive_timeouts = 0;
                    summary.copy_gone += 1;
                    metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_COPY_GONE)
                        .increment(1);
                    warn!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "instrument_snapshot_copy_gone",
                        file = %candidate.file_name,
                        bucket = store.bucket_name(),
                        "instrument snapshot marker named a copy the cold bucket no longer \
                         holds; the marker is dropped and the file is copied again (the \
                         sweep keeps it until that copy is verified)"
                    );
                    if !drop_stale_marker(&candidate.path).await {
                        // The marker still matches the file. The sweep keeps
                        // the file regardless: it deletes only what this pass
                        // proves (`proven`), so it goes only if the upload
                        // below succeeds and verifies (which also rewrites the
                        // marker).
                        summary.stale_marker_kept += 1;
                        warn!(
                            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                            source = "instrument_snapshot_stale_marker_kept",
                            file = %candidate.file_name,
                            "instrument snapshot marker for a copy the cold bucket no \
                             longer holds could not be removed; the file is kept until \
                             a copy is verified again"
                        );
                    }
                }
                CopyRecheck::NoMarker => {}
                CopyRecheck::Unconfirmed { reason, timed_out } => {
                    if timed_out {
                        consecutive_timeouts += 1;
                    } else {
                        consecutive_timeouts = 0;
                    }
                    summary.unconfirmed += 1;
                    metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_RECHECK_FAILED)
                        .increment(1);
                    warn!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "instrument_snapshot_recheck_failed",
                        file = %candidate.file_name,
                        reason = %reason,
                        "instrument snapshot copy could not be re-checked; the file is \
                         kept and this pass sweeps nothing"
                    );
                    continue;
                }
            }
        }
        let key = instrument_snapshot_key(&candidate.date, &candidate.file_name);
        match upload_file(store, &markers_dir, &candidate.path, &key).await {
            SegmentOutcome::Uploaded { key } => {
                consecutive_timeouts = 0;
                summary.uploaded += 1;
                summary.proven.insert(candidate.file_name.clone());
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
                summary.proven.insert(candidate.file_name.clone());
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
                summary.proven.insert(candidate.file_name.clone());
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
    let skips = match gate.request_pass() {
        PassRequest::Claimed => return true,
        PassRequest::Queued(skips) => skips,
    };
    metrics::counter!(SNAPSHOT_UPLOAD_COUNTER, "outcome" => OUTCOME_SKIPPED_BUSY).increment(1);
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

/// One request's work: resolve the bucket, one bounded pass, then (only when
/// that pass re-checked every marked copy against the bucket) the retention
/// sweep for `today_ist`.
async fn one_snapshot_request(bucket: &str, today_ist: &str) {
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
    let Some(summary) = run_bounded_snapshot_pass(&store, dir, limit).await else {
        return;
    };
    info!(
        bucket = store.bucket(),
        candidates = summary.candidates,
        already_copied = summary.already_copied,
        copy_gone = summary.copy_gone,
        unconfirmed = summary.unconfirmed,
        uploaded = summary.uploaded,
        sidecar = summary.sidecar,
        reused = summary.reused,
        failed = summary.failed,
        unreadable = summary.unreadable,
        not_attempted = summary.not_attempted,
        stale_marker_kept = summary.stale_marker_kept,
        proven = summary.proven.len(),
        "instrument snapshot pass finished"
    );
    sweep_after_rechecked_pass(
        &summary,
        today_ist,
        crate::dhan_universe::sweep_artifacts_after_verified_copies,
    );
}

/// Runs `sweep` for `today_ist` only when `summary` says the pass re-checked
/// every marked copy against the bucket ([`SnapshotPassSummary::sweep_allowed`]),
/// handing it the names this pass proved ([`SnapshotPassSummary::proven`]),
/// the only files it may delete; otherwise logs why it was skipped. Returns
/// whether it ran. O(1).
fn sweep_after_rechecked_pass(
    summary: &SnapshotPassSummary,
    today_ist: &str,
    sweep: impl FnOnce(&str, &std::collections::BTreeSet<String>),
) -> bool {
    if summary.sweep_allowed() {
        sweep(today_ist, &summary.proven);
        return true;
    }
    info!(
        unconfirmed = summary.unconfirmed,
        not_attempted = summary.not_attempted,
        unreadable = summary.unreadable,
        listed = summary.listed,
        "instrument artifact sweep skipped this pass: not every folder entry was \
         read and every marked copy re-checked against the cold bucket; nothing is \
         deleted until one is"
    );
    false
}

/// Starts one snapshot pass over `data/instrument-cache` on its own task,
/// copying to `bucket` (the configured cold bucket; empty means
/// `tv-<env>-cold` from the runtime environment, as for the raw frames),
/// then the retention sweep for `today_ist`. Returns `None` (and starts
/// nothing) while an earlier pass is still running; that pass then runs once
/// more when it ends (with its own date: a rerun queued by the next day's
/// build sweeps with the earlier date, which keeps one day more, never less).
// TEST-EXEMPT: loads the live AWS config chain and the real cold bucket; the
// pass it drives is tested through `run_snapshot_pass` /
// `run_bounded_snapshot_pass` with an in-memory store, the slot and rerun
// through `claim_or_queue_pass` / `drive_snapshot_passes` on a test gate.
pub(crate) fn spawn_snapshot_upload(
    bucket: String,
    today_ist: String,
) -> Option<tokio::task::JoinHandle<()>> {
    if !claim_or_queue_pass(&SNAPSHOT_PASS_GATE) {
        return None;
    }
    Some(tokio::spawn(async move {
        drive_snapshot_passes(&SNAPSHOT_PASS_GATE, || {
            one_snapshot_request(&bucket, &today_ist)
        })
        .await;
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
    /// test can prove a marked file costs one HeadObject and no upload.
    #[derive(Default)]
    struct FakeBucket {
        objects: Mutex<HashMap<String, (u64, String)>>,
        puts: AtomicUsize,
        heads: AtomicUsize,
        fail_puts: bool,
        /// Every HeadObject returns an error (the bucket did not answer).
        fail_heads: bool,
        /// Every HeadObject waits forever (a hung connection).
        hang: bool,
        /// Every PutObject waits forever (HeadObject still answers).
        hang_puts: bool,
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
            if self.fail_heads {
                anyhow::bail!("injected HeadObject failure");
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
            if self.hang_puts {
                std::future::pending::<()>().await;
            }
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

        assert!(first.sweep_allowed(), "{first:?}");

        // Second pass: idempotent; a marked file costs one HeadObject (the
        // re-check that its copy is still there) and no upload.
        let puts = bucket.puts.load(Ordering::SeqCst);
        let heads = bucket.heads.load(Ordering::SeqCst);
        let second = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(second.already_copied, 2, "{second:?}");
        assert_eq!(second.uploaded + second.reused + second.failed, 0);
        assert_eq!(second.copy_gone + second.unconfirmed, 0);
        assert!(second.sweep_allowed(), "{second:?}");
        assert_eq!(bucket.puts.load(Ordering::SeqCst), puts);
        assert_eq!(bucket.heads.load(Ordering::SeqCst), heads + 2);
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
        assert_eq!(
            summary,
            SnapshotPassSummary {
                listed: true,
                ..SnapshotPassSummary::default()
            }
        );
        assert!(
            summary.sweep_allowed(),
            "an empty folder holds nothing to keep"
        );
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
        let rendered = handle.render();
        assert!(
            rendered.contains(&format!(
                "{SNAPSHOT_UPLOAD_COUNTER}{{outcome=\"{OUTCOME_SKIPPED_BUSY}\"}} 2"
            )),
            "{rendered}"
        );
        assert!(
            gate.finish_pass(),
            "the skipped requests are queued as one rerun"
        );
        assert_eq!(
            gate.consecutive_skips.load(Ordering::Acquire),
            0,
            "a rerun resets the skip run"
        );
        assert!(!gate.finish_pass(), "the rerun is taken once");
        assert!(!gate.is_running());
        assert!(
            claim_or_queue_pass(&gate),
            "the slot is free once the pass ends"
        );
        assert!(!gate.finish_pass());
    }

    #[test]
    fn test_request_pass_never_drops_a_request_racing_the_end_of_a_pass() {
        // Regression 2026-10-28 (review): with two flags, a request could
        // find the slot taken, the pass then ended reading no rerun, and the
        // request set its rerun flag afterwards: no pass ran for it. Every
        // order of the two one-step operations now ends with a pass for it.
        // Request first, then the end: the end sees the rerun.
        let gate = PassGate::new();
        assert_eq!(gate.request_pass(), PassRequest::Claimed);
        assert_eq!(gate.request_pass(), PassRequest::Queued(1));
        assert!(gate.finish_pass(), "the queued request is run");
        assert!(gate.is_running(), "the slot stays held for the rerun");
        assert!(!gate.finish_pass());
        // The end first, then the request: the request claims the free slot.
        assert_eq!(gate.request_pass(), PassRequest::Claimed);
        assert!(!gate.finish_pass());
        assert_eq!(gate.request_pass(), PassRequest::Claimed);
        assert!(!gate.finish_pass());
        assert!(!gate.is_running());
    }

    #[test]
    fn test_finish_pass_serves_every_request_under_concurrent_load() {
        // Threads request passes while drivers run and end them. A request
        // is served when a pass STARTS after it. The invariant: once all
        // threads stop, the last pass started after the last request.
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        for _round in 0..50 {
            let gate = Arc::new(PassGate::new());
            let requested = Arc::new(AtomicU64::new(0));
            let served = Arc::new(AtomicU64::new(0));
            let mut threads = Vec::new();
            for _ in 0..4 {
                let gate = Arc::clone(&gate);
                let requested = Arc::clone(&requested);
                let served = Arc::clone(&served);
                threads.push(std::thread::spawn(move || {
                    for _ in 0..500 {
                        requested.fetch_add(1, Ordering::SeqCst);
                        if gate.request_pass() == PassRequest::Claimed {
                            loop {
                                // The pass starts: it covers every request so far.
                                let seen = requested.load(Ordering::SeqCst);
                                served.fetch_max(seen, Ordering::SeqCst);
                                std::thread::yield_now();
                                if !gate.finish_pass() {
                                    break;
                                }
                            }
                        }
                    }
                }));
            }
            for thread in threads {
                thread.join().unwrap();
            }
            assert!(!gate.is_running(), "every pass ended");
            assert_eq!(
                served.load(Ordering::SeqCst),
                requested.load(Ordering::SeqCst),
                "a request was left with no pass after it"
            );
        }
    }

    #[test]
    fn test_spawn_snapshot_upload_starts_nothing_while_a_pass_runs() {
        // The only test touching the process-wide gate.
        assert_eq!(
            SNAPSHOT_PASS_GATE.request_pass(),
            PassRequest::Claimed,
            "a free slot is claimed"
        );
        assert!(
            spawn_snapshot_upload(String::new(), String::new()).is_none(),
            "a request while a pass runs returns before any task is spawned"
        );
        assert!(SNAPSHOT_PASS_GATE.finish_pass(), "and is queued as a rerun");
        assert!(!SNAPSHOT_PASS_GATE.finish_pass());
    }

    #[test]
    fn test_pass_slot_frees_the_gate_when_a_pass_panics() {
        let gate = PassGate::new();
        assert_eq!(gate.request_pass(), PassRequest::Claimed);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = PassSlot::new(&gate);
            panic!("a pass panicked");
        }));
        assert!(result.is_err());
        assert!(
            !gate.is_running(),
            "a panicked pass must not wedge the slot"
        );
        assert_eq!(gate.request_pass(), PassRequest::Claimed);
        assert!(!PassSlot::new(&gate).finish());
        assert!(!gate.is_running());
    }

    #[tokio::test]
    async fn test_drive_snapshot_passes_runs_once_more_for_a_request_during_a_pass() {
        let gate = PassGate::new();
        assert!(claim_or_queue_pass(&gate));
        let runs = AtomicUsize::new(0);
        drive_snapshot_passes(&gate, || {
            let run = runs.fetch_add(1, Ordering::SeqCst);
            if run == 0 {
                // Two requests arrive while the first pass runs: one rerun.
                assert!(!claim_or_queue_pass(&gate));
                assert!(!claim_or_queue_pass(&gate));
            }
            async {}
        })
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the queued request ran");
        assert!(
            !gate.is_running(),
            "the slot is released when the last pass ends"
        );
        // Without a request during the pass, exactly one pass runs.
        assert!(claim_or_queue_pass(&gate));
        let single = AtomicUsize::new(0);
        drive_snapshot_passes(&gate, || {
            single.fetch_add(1, Ordering::SeqCst);
            async {}
        })
        .await;
        assert_eq!(single.load(Ordering::SeqCst), 1);
        assert!(!gate.is_running());
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
        assert_eq!(ALL_OUTCOMES.len(), 8);
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
        // (The same objects, so the two marked files re-check as present.)
        let failing = FakeBucket {
            objects: Mutex::new(bucket.objects.lock().unwrap().clone()),
            fail_puts: true,
            ..FakeBucket::default()
        };
        assert_eq!(run_snapshot_pass(&failing, &dir).await.failed, 1);
        // copy_gone: an empty bucket (a bucket-wide delete). The two marked
        // files are copied again, the third for the first time.
        let empty = FakeBucket::default();
        let refilled = run_snapshot_pass(&empty, &dir).await;
        assert_eq!(
            (refilled.copy_gone, refilled.uploaded),
            (2, 3),
            "{refilled:?}"
        );
        // recheck_failed: a bucket that does not answer HeadObject.
        let blind = FakeBucket {
            fail_heads: true,
            ..FakeBucket::default()
        };
        assert_eq!(run_snapshot_pass(&blind, &dir).await.unconfirmed, 3);

        let rendered = handle.render();
        for (outcome, count) in [
            (OUTCOME_UPLOADED, 5),
            (OUTCOME_REUSED, 2),
            (OUTCOME_FAILED, 1),
            (OUTCOME_COPY_GONE, 2),
            (OUTCOME_RECHECK_FAILED, 3),
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

    #[tokio::test]
    async fn test_recheck_marked_copy_reports_present_gone_and_unconfirmed() {
        let dir = scratch_dir("recheck");
        let name = "dhan-contracts-2026-10-23.json";
        let path = dir.join(name);
        std::fs::write(&path, b"{\"rows\":[1]}").unwrap();
        let bucket = FakeBucket::default();
        assert_eq!(
            recheck_marked_copy(&bucket, &path).await,
            CopyRecheck::NoMarker,
            "no marker yet"
        );
        assert_eq!(run_snapshot_pass(&bucket, &dir).await.uploaded, 1);
        assert_eq!(
            recheck_marked_copy(&bucket, &path).await,
            CopyRecheck::Present
        );
        // The bucket did not answer: unconfirmed, not gone.
        let failing = FakeBucket {
            fail_heads: true,
            ..FakeBucket::default()
        };
        assert!(matches!(
            recheck_marked_copy(&failing, &path).await,
            CopyRecheck::Unconfirmed {
                timed_out: false,
                ..
            }
        ));
        // A hung bucket times out against the per-call budget.
        let hung = FakeBucket {
            hang: true,
            budget: Some(std::time::Duration::from_millis(20)),
            ..FakeBucket::default()
        };
        assert!(matches!(
            recheck_marked_copy(&hung, &path).await,
            CopyRecheck::Unconfirmed {
                timed_out: true,
                ..
            }
        ));
        // A different object length at the key is not our copy.
        let key = instrument_snapshot_key("2026-10-23", name);
        bucket
            .objects
            .lock()
            .unwrap()
            .entry(key.clone())
            .and_modify(|(len, _)| *len += 1);
        assert_eq!(recheck_marked_copy(&bucket, &path).await, CopyRecheck::Gone);
        // Deleted from the bucket (owner Quote 46): gone.
        bucket.objects.lock().unwrap().remove(&key);
        assert_eq!(recheck_marked_copy(&bucket, &path).await, CopyRecheck::Gone);
        // A marker naming another bucket proves nothing about this one.
        let fresh = FakeBucket::default();
        assert_eq!(run_snapshot_pass(&fresh, &dir).await.copy_gone, 1);
        let marker_file = dir.join(UPLOADED_SUBDIR).join(name);
        let text = std::fs::read_to_string(&marker_file).unwrap();
        assert!(text.contains("tv-test-cold"), "{text}");
        std::fs::write(&marker_file, text.replace("tv-test-cold", "tv-other-cold")).unwrap();
        assert_eq!(recheck_marked_copy(&fresh, &path).await, CopyRecheck::Gone);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_run_snapshot_pass_recopies_a_file_whose_copy_was_deleted_from_the_bucket() {
        // Regression 2026-10-28 (review): after a bucket-wide delete (owner
        // Quote 46) the local marker still matched the file, so the sweep
        // would have deleted the only remaining copy. The pass now re-checks
        // the marker against the bucket, drops it, and copies the file again.
        let dir = scratch_dir("gone");
        let name = "dhan-ntm-spot-2026-10-19.json";
        let path = dir.join(name);
        std::fs::write(&path, b"{\"spots\":[9]}").unwrap();
        let bucket = FakeBucket::default();
        assert_eq!(run_snapshot_pass(&bucket, &dir).await.uploaded, 1);
        let key = instrument_snapshot_key("2026-10-19", name);
        bucket.objects.lock().unwrap().clear(); // the bucket-wide delete
        let meta = std::fs::metadata(&path).unwrap();
        assert!(
            CopyGate::Required.allows_delete(&path, &meta),
            "the stale marker alone would let the sweep delete the file"
        );

        let pass = run_snapshot_pass(&bucket, &dir).await;
        assert_eq!(pass.copy_gone, 1, "{pass:?}");
        assert_eq!(pass.uploaded, 1, "copied again in the same pass: {pass:?}");
        assert_eq!(pass.already_copied, 0, "{pass:?}");
        assert!(pass.sweep_allowed(), "{pass:?}");
        assert!(
            bucket.objects.lock().unwrap().contains_key(&key),
            "the copy is back in the bucket before any sweep runs"
        );
        let meta = std::fs::metadata(&path).unwrap();
        assert!(CopyGate::Required.allows_delete(&path, &meta));

        // A bucket that does not answer: the file is kept and no sweep runs.
        let failing = FakeBucket {
            fail_heads: true,
            ..FakeBucket::default()
        };
        let blind = run_snapshot_pass(&failing, &dir).await;
        assert_eq!(blind.unconfirmed, 1, "{blind:?}");
        assert!(!blind.sweep_allowed(), "{blind:?}");
        assert!(path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_sweep_allowed_needs_a_listed_complete_and_rechecked_pass() {
        let clean = SnapshotPassSummary {
            listed: true,
            ..SnapshotPassSummary::default()
        };
        assert!(clean.sweep_allowed());
        assert!(
            !SnapshotPassSummary::default().sweep_allowed(),
            "not listed"
        );
        assert!(
            !SnapshotPassSummary {
                unconfirmed: 1,
                ..clean.clone()
            }
            .sweep_allowed()
        );
        assert!(
            !SnapshotPassSummary {
                not_attempted: 1,
                ..clean.clone()
            }
            .sweep_allowed()
        );
        // A failed upload of an unmarked file does not block the sweep: the
        // sweep keeps every file with no verified copy anyway.
        assert!(
            SnapshotPassSummary {
                failed: 1,
                copy_gone: 1,
                ..clean
            }
            .sweep_allowed()
        );
    }

    #[test]
    fn test_sweep_after_rechecked_pass_runs_only_after_a_full_recheck() {
        let ran = std::cell::RefCell::new(Vec::new());
        let clean = SnapshotPassSummary {
            listed: true,
            ..SnapshotPassSummary::default()
        };
        assert!(sweep_after_rechecked_pass(&clean, "2026-10-28", |d, _| ran
            .borrow_mut()
            .push(d.to_string())));
        let blind = SnapshotPassSummary {
            unconfirmed: 2,
            ..clean
        };
        assert!(!sweep_after_rechecked_pass(&blind, "2026-10-29", |d, _| {
            ran.borrow_mut().push(d.to_string())
        }));
        assert_eq!(*ran.borrow(), ["2026-10-28"]);
    }

    /// The sweep, as production calls it, over `dir`, with the proven names
    /// the pass hands it.
    fn sweep_dir(
        dir: &Path,
        summary: &SnapshotPassSummary,
        today: &str,
    ) -> Option<crate::dhan_universe::SweepTally> {
        let mut tally = None;
        sweep_after_rechecked_pass(summary, today, |day, proven| {
            tally = Some(crate::dhan_universe::sweep_stale_artifacts_in(
                dir,
                day,
                CopyGate::Required,
                proven,
            ));
        });
        tally
    }

    /// Review 2026-10-28, finding 1: the copy is gone from the bucket, the
    /// unlink of its marker fails (the marker survives and still matches the
    /// file), and the re-upload fails or times out. The sweep must keep the
    /// file: it deletes only what this pass proved, never on a marker alone.
    /// The positive control (re-upload succeeds) is deleted.
    #[tokio::test]
    async fn test_run_snapshot_pass_keeps_a_file_whose_stale_marker_survived_a_failed_reupload() {
        let name = "dhan-contracts-1999-01-01.json";
        let cases = [
            (
                "gone_put_failed",
                FakeBucket {
                    fail_puts: true,
                    ..FakeBucket::default()
                },
                false,
            ),
            (
                "gone_put_timed_out",
                FakeBucket {
                    hang_puts: true,
                    budget: Some(Duration::from_millis(50)),
                    ..FakeBucket::default()
                },
                false,
            ),
            ("gone_put_ok", FakeBucket::default(), true),
        ];
        for (tag, bucket, reupload_lands) in cases {
            let dir = scratch_dir(tag);
            let path = dir.join(name);
            std::fs::write(&path, b"{\"rows\":[1]}").unwrap();
            // The first copy lands in another (same-named) bucket and is marked.
            assert_eq!(
                run_snapshot_pass(&FakeBucket::default(), &dir)
                    .await
                    .uploaded,
                1,
                "{tag}"
            );
            let marker_path = dir.join(UPLOADED_SUBDIR).join(name);
            let marker_body = std::fs::read(&marker_path).unwrap();

            // `bucket` is empty: the copy is gone (a bucket-wide delete).
            let pass = run_snapshot_pass(&bucket, &dir).await;
            assert_eq!(pass.copy_gone, 1, "{tag}: {pass:?}");
            assert_eq!(pass.stale_marker_kept, 0, "{tag}: {pass:?}");
            assert_eq!(
                pass.proven.contains(name),
                reupload_lands,
                "{tag}: {pass:?}"
            );
            assert!(pass.sweep_allowed(), "{tag}: {pass:?}");
            if !reupload_lands {
                assert_eq!(pass.failed, 1, "{tag}: {pass:?}");
                // The state a failed unlink leaves: the old marker is there
                // and still matches the file's length and mtime.
                assert!(!marker_path.exists(), "{tag}: the real unlink ran");
                std::fs::write(&marker_path, &marker_body).unwrap();
                let meta = std::fs::metadata(&path).unwrap();
                assert!(
                    CopyGate::Required.allows_delete(&path, &meta),
                    "{tag}: the marker alone would let the sweep delete the file"
                );
            }

            let tally = sweep_dir(&dir, &pass, "1999-01-09").expect("the sweep ran");
            assert_eq!(
                path.exists(),
                !reupload_lands,
                "{tag}: kept unless this pass proved the copy: {tally:?}"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// `drop_stale_marker` reports a marker it could not remove (here a
    /// directory where the marker file belongs, which `unlink` refuses even
    /// for root) instead of dropping the error, and `true` once the marker
    /// is gone or was never there.
    #[tokio::test]
    async fn test_drop_stale_marker_reports_a_marker_it_could_not_remove() {
        let dir = scratch_dir("drop_marker");
        let markers = dir.join(UPLOADED_SUBDIR);
        std::fs::create_dir_all(&markers).unwrap();

        let stuck = dir.join("dhan-contracts-1999-01-01.json");
        std::fs::create_dir_all(markers.join("dhan-contracts-1999-01-01.json").join("x")).unwrap();
        assert!(!drop_stale_marker(&stuck).await, "an unremovable marker");
        assert!(!marker_file_absent(&stuck));

        let removable = dir.join("dhan-nse-mapping-1999-01-01.json");
        std::fs::write(markers.join("dhan-nse-mapping-1999-01-01.json"), b"{}").unwrap();
        assert!(!marker_file_absent(&removable));
        assert!(drop_stale_marker(&removable).await, "a removed marker");
        assert!(marker_file_absent(&removable));

        let never = dir.join("dhan-ntm-spot-1999-01-01.json");
        assert!(drop_stale_marker(&never).await, "no marker at all");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Review 2026-10-28, finding 2: a folder entry the listing could not
    /// read was never re-checked, so it blocks the whole sweep, even when
    /// every other file was proven.
    #[test]
    fn test_sweep_allowed_is_false_while_any_folder_entry_was_unread() {
        let clean = SnapshotPassSummary {
            listed: true,
            ..SnapshotPassSummary::default()
        };
        assert!(clean.sweep_allowed());
        let unread = SnapshotPassSummary {
            unreadable: 1,
            ..clean
        };
        assert!(!unread.sweep_allowed(), "{unread:?}");
    }

    /// Finding 2, end to end: with an unread entry the sweep does not run,
    /// so even a proven file with a matching marker stays. The same summary
    /// with the entry read DOES delete that file (the control below), so the
    /// `exists()` check fails if the unread-entry block is ever removed.
    #[tokio::test]
    async fn test_sweep_after_rechecked_pass_keeps_every_file_after_an_unread_entry() {
        let proven_name = "dhan-contracts-1999-01-01.json";
        let other_name = "dhan-nse-mapping-1999-01-01.json";
        for unreadable in [1_usize, 0] {
            let dir = scratch_dir("unread_entry");
            std::fs::write(dir.join(proven_name), b"{\"rows\":[1]}").unwrap();
            // A real pass uploads and marks the proven file.
            let pass = run_snapshot_pass(&FakeBucket::default(), &dir).await;
            assert!(pass.proven.contains(proven_name), "{pass:?}");
            std::fs::write(dir.join(other_name), b"{}").unwrap();
            let summary = SnapshotPassSummary { unreadable, ..pass };
            let tally = sweep_dir(&dir, &summary, "1999-01-09");
            if unreadable > 0 {
                assert!(tally.is_none(), "no sweep after an unread entry");
                assert!(
                    dir.join(proven_name).exists(),
                    "kept: the sweep did not run"
                );
            } else {
                assert!(tally.is_some(), "control: the sweep runs");
                assert!(
                    !dir.join(proven_name).exists(),
                    "control: a proven, marked file is swept when every entry was read"
                );
            }
            assert!(dir.join(other_name).exists(), "never proven, never swept");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Review 2026-10-28, finding 3: the gap between a file's proof and the
    /// sweep is bounded by the pass bound, not "seconds". The header states
    /// that bound and its value; a pass past it never sweeps.
    #[tokio::test]
    async fn test_run_bounded_snapshot_pass_bounds_the_gap_between_proof_and_sweep() {
        assert_eq!(SNAPSHOT_PASS_TIMEOUT_SECS, 1_800, "the header says 1,800 s");
        let src = include_str!("instrument_snapshot_upload.rs");
        let header: String = src
            .lines()
            .take_while(|l| l.starts_with("//!"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!header.contains("(seconds)"), "the gap is not seconds");
        assert!(
            header.contains("[`SNAPSHOT_PASS_TIMEOUT_SECS`] (1,800 s, 30 minutes"),
            "the honest limit must name the real bound"
        );
        // A pass that runs past the bound yields no summary, so no sweep.
        let dir = scratch_dir("bound");
        std::fs::write(dir.join("dhan-contracts-1999-01-01.json"), b"{}").unwrap();
        let hung = FakeBucket {
            hang_puts: true,
            ..FakeBucket::default()
        };
        assert!(
            run_bounded_snapshot_pass(&hung, &dir, Duration::from_millis(50))
                .await
                .is_none()
        );
        assert!(dir.join("dhan-contracts-1999-01-01.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
