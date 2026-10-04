//! Wave 6 Sub-PR #1 item 1.2f.3 — sealed-candle writer-task drain logic.
//!
//! The COLD-PATH consumer that drains the [`SealAbsorptionPipeline`]
//! ring, batches into the [`ShadowCandleWriter`] ILP buffer, attempts
//! a flush, and on flush failure rescues the in-flight seals via
//! [`SealAbsorptionPipeline::rescue_in_flight`] (escalating to disk
//! spill → NDJSON DLQ → `Dropped`).
//!
//! ## Why a sync `drain_once` and not a tokio task here
//!
//! Per Wave-6 plan Item 1.2f.3 — the SLICE that lands the actual
//! `tokio::spawn` loop + interval timer + cancellation token is item
//! 1.2f.4. THIS slice ships the pure synchronous drain function so
//! the rescue cascade is testable end-to-end without spinning up a
//! tokio runtime + async test harness.
//!
//! The eventual tokio loop in 1.2f.4 will be a thin shell that:
//! ```ignore
//! loop {
//!     tokio::time::sleep(SEAL_DRAIN_INTERVAL).await;
//!     let outcome = drain_once(&mut pipeline, &mut writer, MAX_DRAIN, now_unix_secs());
//!     // … emit Prom counters per outcome.field …
//! }
//! ```
//!
//! ## What this slice ships
//!
//! - [`DrainOutcome`] — counts every outcome category for one drain
//!   cycle (idle, flushed, rescued-to-spill, rescued-to-dlq,
//!   rescued-dropped). Maps 1:1 to the future
//!   `tv_seal_writer_drain_total{kind=...}` Prometheus counter.
//! - [`drain_once`] — the function itself (sync, single drain cycle).
//! - 11 unit tests covering: idle path; bounded drain
//!   (`max_drain` cap); flush-fail-rescue-to-spill (full cascade
//!   verified by inspecting the spill file); flush-fail-rescue-to-dlq
//!   (spill blocked → DLQ catches); flush-fail-rescue-dropped (both
//!   blocked → caller-must-fire AGGREGATOR-DROP-01); ring drained to
//!   zero on rescue (no FIFO inversion); pending_count semantics on
//!   rescue.
//!
//! ## Hot-path safety
//!
//! `drain_once` is COLD path (writer task only). It is allowed to
//! allocate (`Vec::with_capacity(max_drain)` rescue buffer) — the
//! hot-path zero-alloc rule applies to `MultiTfAggregator::consume_tick`
//! and below, NOT to the storage drain task.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use tracing::{error, info, warn};

use tickvault_common::error_code::ErrorCode;
use tickvault_trading::candles::BufferedSeal;

use crate::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use crate::seal_dlq::SealDlqRecord;
use crate::seal_spill::{
    SEAL_BOOT_WRITTEN_CAPACITY, SEAL_SPILL_FORMAT_VERSION, SEAL_SPILL_RECORD_SIZE, SerializedSeal,
    SpillRecordRead, decode_spill_record, seal_spill_version_is_readable,
};
use crate::seal_spill_ledger::BootWritten;
use crate::shadow_candle_writer::ShadowCandleWriter;

/// Outcome counters for one [`drain_once`] cycle. Maps 1:1 to the
/// future `tv_seal_writer_drain_total{kind="ring_pop"|"flushed"|...}`
/// Prometheus counter labels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DrainOutcome {
    /// Number of seals popped from the ring this cycle.
    /// `0` means the ring was empty (idle path).
    pub ring_seals_popped: usize,
    /// `true` if the ILP `flush()` returned `Ok(())` and the popped
    /// seals are now committed to QuestDB.
    pub flushed_ok: bool,
    /// On flush failure: how many of the in-flight seals landed in
    /// the disk spill tier (binary fixed-record file).
    pub rescued_to_spill: usize,
    /// On flush failure: how many landed in the NDJSON DLQ tier
    /// (because spill was full / failed).
    pub rescued_to_dlq: usize,
    /// On flush failure: how many were truly dropped (all 3 tiers
    /// failed). Caller MUST fire
    /// `error!(code = ErrorCode::AggregatorDrop01.code_str(), …)`
    /// per the AGGREGATOR-DROP-01 runbook for each.
    pub rescued_dropped: usize,
}

impl DrainOutcome {
    /// `true` if no seals were popped (the ring was empty).
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.ring_seals_popped == 0
    }

    /// Folds another cycle's outcome into this one, for callers that run
    /// several cycles and must report the TOTAL rather than the last.
    ///
    /// `flushed_ok` is `AND`ed, not `OR`ed: across a multi-cycle drain the
    /// honest reading of "did the data reach QuestDB" is *every* flush
    /// succeeded. `OR` would let one good cycle mask a failing one, which is
    /// the false-OK this codebase keeps having to remove.
    ///
    /// # Complexity
    /// O(1) — five saturating adds and a boolean.
    pub const fn accumulate(&mut self, other: &Self) {
        self.ring_seals_popped = self
            .ring_seals_popped
            .saturating_add(other.ring_seals_popped);
        self.flushed_ok = self.flushed_ok && other.flushed_ok;
        self.rescued_to_spill = self.rescued_to_spill.saturating_add(other.rescued_to_spill);
        self.rescued_to_dlq = self.rescued_to_dlq.saturating_add(other.rescued_to_dlq);
        self.rescued_dropped = self.rescued_dropped.saturating_add(other.rescued_dropped);
    }

    /// `true` if any seals were rescued to spill / DLQ / dropped
    /// because flush failed.
    #[must_use]
    pub const fn has_rescues(&self) -> bool {
        self.rescued_to_spill > 0 || self.rescued_to_dlq > 0 || self.rescued_dropped > 0
    }
}

/// Drain up to `max_drain` seals from the [`SealAbsorptionPipeline`]
/// ring, batch them into the [`ShadowCandleWriter`] ILP buffer,
/// attempt a flush, and on flush failure rescue the in-flight seals
/// down the spill → DLQ → drop cascade.
///
/// Single drain cycle — caller is responsible for calling this in a
/// loop on whatever cadence is appropriate (the future tokio task in
/// item 1.2f.4 sleeps `SEAL_DRAIN_INTERVAL` between cycles).
///
/// **Cold path.** Allowed to allocate; allocates one
/// `Vec::with_capacity(max_drain)` rescue buffer per call. Reused
/// across calls in the future tokio loop via a writer-task struct
/// holding the buffer; this slice's pure function takes the simpler
/// once-per-call allocation.
///
/// **No FIFO inversion on rescue.** Popped seals are NOT pushed back
/// into the ring (which would put them at the BACK and invert
/// drain order). Instead they go straight to disk spill via
/// [`SealAbsorptionPipeline::rescue_in_flight`].
///
/// `now_unix_secs` is the UTC unix timestamp passed by the caller
/// per locked decision **L-H7** — never `Utc::now()` here.
pub fn drain_once(
    pipeline: &mut SealAbsorptionPipeline,
    writer: &mut ShadowCandleWriter,
    max_drain: usize,
    now_unix_secs: i64,
) -> DrainOutcome {
    let mut outcome = DrainOutcome::default();

    // Idle short-circuit.
    if pipeline.ring_len() == 0 || max_drain == 0 {
        return outcome;
    }

    // Pop up to max_drain seals into the rescue buffer. We pop FIRST
    // (so the ring is drained), THEN attempt flush. If flush fails,
    // every popped seal cascades to spill/DLQ/drop — they do NOT
    // re-enter the ring (would invert FIFO).
    // Sized to what is ACTUALLY waiting, not to the cap. With the cap at
    // 1,024 this distinction did not matter; at 16,384 a mostly-empty ring
    // would otherwise reserve ~2.4 MB ten times a second for a handful of
    // seals. `ring_len` is the real bound on how many `pop_oldest` can yield
    // this cycle, and the `.min` keeps the reservation exact in both
    // directions — no over-reserve when idle, no re-alloc when saturated.
    let mut popped: Vec<BufferedSeal> = Vec::with_capacity(max_drain.min(pipeline.ring_len()));
    while popped.len() < max_drain {
        match pipeline.pop_oldest() {
            Some(seal) => {
                if let Err(append_err) = writer.append_seal(&seal) {
                    // ILP buffer-fill error (extremely rare — column
                    // type / table-name issue). Rescue this one
                    // immediately and stop draining further.
                    // Finding S3: seal-time ILP append failure is a
                    // persist failure — logged at `error!` with the
                    // AGGREGATOR-SEAL-01 code per
                    // `error_level_meta_guard.rs` Rule 5.
                    error!(
                        code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                        ?append_err,
                        security_id = seal.security_id,
                        "candle append_seal failed — rescuing in-flight"
                    );
                    rescue_one(pipeline, &mut outcome, seal, now_unix_secs);
                    break;
                }
                popped.push(seal);
            }
            None => break, // ring exhausted
        }
    }

    outcome.ring_seals_popped = popped.len();

    // CAP SATURATION — the signal whose absence let a live ceiling hide.
    //
    // Added 2026-08-20. On the prod box the drain sat at EXACTLY
    // `cycles × max_drain` for hours while 61% of sealed candles went to
    // disk, and every counter that existed looked reasonable: seals were
    // submitted, rows were written, `flush_failures` was 0, `dropped` was 0.
    // The only way to see it was to notice that `rows_written` was an exact
    // multiple of 1024 — arithmetic nobody runs on a dashboard.
    //
    // A cap that binds is a different condition from a cap that fits, and
    // until now they produced identical telemetry. This one says which:
    // non-zero means the ring had MORE waiting than we were allowed to take,
    // so the overflow went to spill because of a CONSTANT, not because the
    // database refused it. `flush_failures` stays the signal for the database
    // genuinely being unable to keep up; these two must never be confused,
    // because they have opposite fixes.
    if outcome.ring_seals_popped == max_drain && pipeline.ring_len() > 0 {
        metrics::counter!("tv_seal_drain_cap_saturated_total").increment(1);
    }

    // Idle if nothing landed in the writer (ring was racing-empty by
    // the time we started, or every append failed).
    if outcome.ring_seals_popped == 0 {
        return outcome;
    }

    // Attempt the flush. On success: drop the rescue buffer (all
    // committed). On failure: cascade EVERY popped seal through the
    // rescue path.
    match writer.flush() {
        Ok(()) => {
            outcome.flushed_ok = true;
            // Audit PR41a: an amended bar committed live while its original
            // waits in the spill. Append the fuller copy behind it, so no
            // replay ends on the original.
            pipeline.note_live_commits(&popped, now_unix_secs);
        }
        Err(flush_err) => {
            // Finding S3: seal-time ILP flush failure is a persist
            // failure — logged at `error!` with the AGGREGATOR-SEAL-01
            // code per `error_level_meta_guard.rs` Rule 5.
            //
            // Audit PR31a: a refusal because the candle tables are not yet
            // keyed was already logged once, coded, by the writer; repeating
            // it here would be a coded `error!` every 100 ms until the ensure
            // succeeds. The seals take the same rescue path below.
            if flush_err.is::<crate::shadow_candle_writer::CandleTablesNotKeyed>() {
                tracing::debug!(
                    count = popped.len(),
                    "candle flush refused (tables not yet keyed) — rescuing to spill"
                );
            } else {
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    ?flush_err,
                    count = popped.len(),
                    "candle flush failed — rescuing in-flight seals to spill/DLQ"
                );
            }
            for seal in popped {
                rescue_one(pipeline, &mut outcome, seal, now_unix_secs);
            }
            // Poison-buffer recovery (2026-07-06 hostile-review fix): the
            // popped seals are now durably parked in spill/DLQ (or counted as
            // Dropped — AGGREGATOR-DROP-01 fires at the loop), so the writer's
            // retained ILP buffer is redundant AND toxic:
            // - a server-REJECTED (non-connection) row would otherwise replay
            //   on EVERY later flush, permanently killing candle persistence
            //   for the session, and
            // - during a sustained connection outage the buffer would grow
            //   without bound (each cycle appends more already-rescued rows)
            //   until the questdb-rs 100 MiB max_buf_size wedges every flush
            //   with a pre-transport error — forever, even after recovery.
            // Discard so the next cycle starts clean; DEDUP keys absorb any
            // partially-committed rows if the failure raced a server commit.
            writer.discard_pending();
        }
    }

    outcome
}

/// Walks one in-flight seal through the rescue cascade and updates
/// the `DrainOutcome` counters.
fn rescue_one(
    pipeline: &SealAbsorptionPipeline,
    outcome: &mut DrainOutcome,
    seal: BufferedSeal,
    now_unix_secs: i64,
) {
    match pipeline.rescue_in_flight(seal, now_unix_secs) {
        SubmitOutcome::Spilled => outcome.rescued_to_spill += 1,
        SubmitOutcome::DlqWritten => outcome.rescued_to_dlq += 1,
        SubmitOutcome::Dropped(_) => outcome.rescued_dropped += 1,
        // SubmitOutcome::Buffered cannot occur via rescue_in_flight
        // (it skips the ring). If it ever does, treat as a logic
        // bug and surface for triage; for now we count nothing.
        SubmitOutcome::Buffered => {}
    }
}

// ---------------------------------------------------------------------------
// Boot-time recovery drain (2026-08-11 — closes the silent-loss gap)
// ---------------------------------------------------------------------------
//
// ## The gap this closes
//
// `drain_once` above rescues sealed candles to `SealSpillWriter` /
// `SealDlqWriter` whenever the ILP flush fails. Both writers have always
// carried a `read_all` + `clear_*_for_date` pair, and both doc-comments say
// "the caller (writer task) deletes the spill file after `read_all` is fully
// replayed". **That caller never existed.** Verified 2026-08-11: every
// `read_all` call site in `crates/*/src` sits inside its own file's
// `#[cfg(test)] mod tests` block, and no crate outside `storage` references
// the spill/DLQ types at all. So every candle rescued during a QuestDB outage
// landed in a file that nothing ever read back — permanent, silent loss, while
// `tv_seal_writer_drain_total{kind="rescued_spill"}` reported it ABSORBED.
//
// ## The recovery contract (two-phase, mirrors `ws_frame_spill`)
//
// Lifted verbatim in shape from the WAL replay staging pattern
// (`ws_frame_spill.rs` — the `replaying/` → confirm → `archive/` chain):
//
// 1. **Stage.** Every `seals_v4-*` (and legacy `seals-*`) file in the
//    spill / DLQ dir is MOVED into
//    `<dir>/replaying/`. Leftovers already sitting in `replaying/` from a
//    prior crashed boot are re-globbed too, so a crash mid-recovery loses
//    nothing.
// 2. **Re-ingest.** Records are decoded and appended straight into the ILP
//    writer, flushed in bounded batches.
// 3. **Confirm.** ONLY after a successful flush does the file move
//    `replaying/` → `archive/`. `archive/` is never re-globbed, so confirmed
//    history never re-injects.
// 4. **Fail-closed.** A failed flush leaves the file in `replaying/` and
//    STOPS the drain (QuestDB is down; the next file would just burn another
//    bounded flush timeout). Next boot re-globs it.
//
// ## Why this is idempotent
//
// Two independent layers, because either one alone is insufficient:
//
// - **File side:** a file is in exactly ONE of `replaying/` (not yet
//   confirmed) or `archive/` (confirmed). The rename is the commit point, so
//   a crash anywhere re-reads at most the one in-flight file — never a file
//   already archived. Recovered seals are deliberately NOT re-submitted into
//   the absorption pipeline: doing so would let a still-dead QuestDB rescue
//   them into a FRESH spill file while the staged copy also survives,
//   multiplying the on-disk set on every failed boot.
// - **DB side:** the candle tables' DEDUP UPSERT KEYS
//   `(ts, security_id, segment, feed)` collapse a re-ingest of the same seal,
//   which covers the residual window where a flush commits server-side but
//   the process dies before the archive rename.
//
// ## Honesty (Rule 11 — no false-OK)
//
// `seals_left_pending` is reported to the caller and surfaced as a counter +
// an `error!`. Seals sitting in `replaying/` are on DISK and NOT in QuestDB;
// this module never reports them as absorbed.

/// Staging directory for files that have been read back but whose re-ingest
/// is not yet confirmed. Re-globbed on every boot until confirmed.
pub const SEAL_REPLAYING_SUBDIR: &str = "replaying";

/// Confirmed-history directory. NEVER re-globbed, so an archived file can
/// never re-inject.
pub const SEAL_ARCHIVE_SUBDIR: &str = "archive";

/// Filename prefix of the files the CURRENT writers produce, for both the
/// spill (`.bin`) and the DLQ (`.ndjson`): `seals_v4-YYYY-MM-DD.*`.
///
/// ⚠ Renamed 2026-09-22 from `seals-` (hostile finding B2). Version 4
/// renumbered the timeframe ordinals, and every earlier binary selects files
/// by `name.starts_with("seals-")` and refuses only version-0 records. So after a
/// rollback, an older binary would read a v4 file and file each bar under the
/// OLD ordinal: an S3 bar into `candles_1s`, and an M30 bar into `candles_3s`.
/// `seals_v4-` does not start with `seals-`, so an older binary never opens
/// these files. A rollback strands them on disk, still recoverable, instead of
/// corrupting two tables without logging anything.
pub const SEAL_FILE_PREFIX: &str = "seals_v4-";

/// The pre-v4 prefix. Still globbed, so files written by an older binary are
/// staged, REFUSED by the version gate (every record in them has a stale
/// `format_version`), and archived. They are never stranded and never re-read.
pub const LEGACY_SEAL_FILE_PREFIX: &str = "seals-";

// The whole rollback guarantee rests on this: the current prefix must not
// match an older binary's `starts_with("seals-")` test.
const _: () = assert!(!starts_with_const(
    SEAL_FILE_PREFIX,
    LEGACY_SEAL_FILE_PREFIX
));

const fn starts_with_const(s: &str, prefix: &str) -> bool {
    let (s, p) = (s.as_bytes(), prefix.as_bytes());
    if p.len() > s.len() {
        return false;
    }
    let mut i = 0;
    while i < p.len() {
        if s[i] != p[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Outcome of one [`drain_recovered_seals`] pass. Every field maps to a
/// `tv_seal_writer_drain_total{kind=...}` label emitted by the writer loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootDrainOutcome {
    /// Files moved into (or already sitting in) `replaying/` this pass.
    pub files_staged: usize,
    /// Records successfully decoded off disk.
    pub seals_recovered: usize,
    /// Recovered seals that a successful flush committed to QuestDB.
    pub seals_reingested: usize,
    /// Files confirmed and moved to `archive/`.
    pub files_archived: usize,
    /// Files still in `replaying/` (flush failed or never attempted).
    pub files_left_pending: usize,
    /// Seals still on disk and NOT in QuestDB. The honest loss-risk number.
    pub seals_left_pending: usize,
    /// Records that could not be decoded (corrupt tail / legacy format /
    /// unknown timeframe ordinal). Their bytes survive in `archive/`.
    pub records_undecodable: usize,
    /// Decoded seals the writer refused to append. Their file is NOT archived
    /// (audit PR31): it stays in `replaying/` so the next boot retries it, and
    /// these seals are also counted in `seals_left_pending`.
    pub seals_append_failed: usize,
    /// Audit PR41a: decoded seals not written because this drain had already
    /// written a fuller copy of the same bucket. The fuller copy is the one
    /// the database keeps.
    pub seals_superseded: usize,
    /// Z6: recovered seals written without being recorded, because the
    /// drain's record of written bars was full. An older copy of such a bar
    /// read later in the drain is written too.
    pub seals_untracked: usize,
    /// S1: bars seeded from the summary an earlier, stopped drain left
    /// behind, so a file it left staged cannot replace them with an older copy.
    pub summary_seeded: usize,
    /// S1: summary files refused (damaged or unreadable) or bars in one the
    /// record had no room for. A refused summary means this drain runs without
    /// the earlier boot's guard, as every drain did before.
    pub summary_refused: usize,
    /// S1: committed bars this drain could NOT write to its summary (bound
    /// reached, or the write failed) although files stay staged. A later boot
    /// may write an older copy of such a bar.
    pub summary_not_persisted: usize,
}

impl BootDrainOutcome {
    /// `true` if nothing was found on disk (the healthy fresh-boot path).
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.files_staged == 0
    }
}

/// Which on-disk format a staged file carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StagedKind {
    /// Fixed 128-byte binary records (`SealSpillWriter`).
    Spill,
    /// NDJSON lines (`SealDlqWriter`).
    Dlq,
}

/// Moves every `seals_v4-*` (and legacy `seals-*`) file in `dir` into
/// `dir/replaying/` and returns the
/// full staged set (including leftovers from a prior crashed boot).
///
/// A file that cannot be moved is skipped with a `warn!` and left in place —
/// it is retried on the next boot. Never deletes, never loses.
fn stage_pending_files(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let replaying = dir.join(SEAL_REPLAYING_SUBDIR);
    if let Err(err) = std::fs::create_dir_all(&replaying) {
        warn!(?replaying, ?err, "cannot create seal replay staging dir");
        return Vec::new();
    }

    // Move live files into staging. `rename` within one directory tree is
    // atomic, so a crash leaves the file in exactly one of the two places.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_seal_file(&path) {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            let target = replaying.join(name);
            if target.exists() {
                // A same-named leftover is already staged (prior crashed
                // boot). Keep BOTH: park the live one under a free name
                // rather than clobbering un-recovered records.
                let target = free_path(&replaying, name);
                if let Err(err) = std::fs::rename(&path, &target) {
                    warn!(?path, ?err, "cannot stage seal file — retried next boot");
                }
                continue;
            }
            if let Err(err) = std::fs::rename(&path, &target) {
                warn!(?path, ?err, "cannot stage seal file — retried next boot");
            }
        }
    }

    // Re-glob the staging dir: this pass's moves PLUS any unconfirmed
    // leftovers, oldest write first.
    let mut staged: Vec<PathBuf> = std::fs::read_dir(&replaying)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_seal_file(p))
        .collect();
    sort_by_write_time(&mut staged);
    staged
}

/// Audit PR41a: order staged files oldest write first, name second.
///
/// Name order alone put a day file before the file set aside from the SAME
/// day (`<name>.<n>`, `set_aside_torn_file`), although every record in the
/// set-aside file was written first. A replay in that order wrote the older
/// records last. A file whose write time cannot be read sorts first, by name,
/// which is the order every file had before.
///
/// # Complexity
/// O(files log files) plus one `stat` per file. Cold: a directory listing.
fn sort_by_write_time(files: &mut [PathBuf]) {
    // O(1) EXEMPT: cold directory listing, once per scan or boot
    files.sort_by_cached_key(|path| {
        let written = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        (written, path.clone())
    });
}

/// `true` for a regular file named `seals_v4-*` or legacy `seals-*`, ending `.bin` or `.ndjson`.
fn is_seal_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    (name.starts_with(SEAL_FILE_PREFIX) || name.starts_with(LEGACY_SEAL_FILE_PREFIX))
        && staged_kind(path).is_some()
}

/// Classifies a staged file by extension. Collision-suffixed names
/// (`seals-2026-08-11.bin.1`) keep their kind via the embedded extension.
fn staged_kind(path: &Path) -> Option<StagedKind> {
    let name = path.file_name().and_then(|n| n.to_str())?;
    // Split off every `.N` collision suffix and the `.overflow` suffix
    // `free_path` falls back to before classifying (PR40b-f: until 2026-10-04
    // only one `.N` was split, so an `.overflow` file or a copy renamed twice
    // was never staged, read or replayed).
    let base = crate::seal_spill::strip_copy_suffixes(name);
    if base.ends_with(".bin") {
        Some(StagedKind::Spill)
    } else if base.ends_with(".ndjson") {
        Some(StagedKind::Dlq)
    } else {
        None
    }
}

/// Returns `dir/name`, appending `.1`, `.2`, … until the path is free.
/// Bounded so a pathological directory cannot spin forever.
fn free_path(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let base = dir.join(name);
    if !base.exists() {
        return base;
    }
    let stem = name.to_string_lossy().into_owned();
    for suffix in 1..10_000u32 {
        let candidate = dir.join(format!("{stem}.{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}.overflow"))
}

/// Decodes every fixed-size record in a staged spill file.
/// Returns `(records, undecodable_count)`.
fn read_staged_spill(path: &Path) -> Option<(Vec<SerializedSeal>, usize)> {
    let Ok(file) = std::fs::File::open(path) else {
        // `None`, NOT an empty vec. Until 2026-08-21 both arms returned
        // `(Vec::new(), 0)`, which is indistinguishable from "read fine, found
        // nothing" — so the caller archived the file as fully recovered and
        // the seals inside it were lost permanently, with a success line in
        // the boot log. The caller now refuses to archive on `None`.
        warn!(?path, "cannot open staged spill file");
        return None;
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut undecodable = 0usize;
    let mut checksum_refused = 0usize;
    let mut buf = [0u8; SEAL_SPILL_RECORD_SIZE];
    // The loop ends on the first read error, which is either a clean EOF or a
    // truncated trailing record (a torn write at the moment of the crash).
    // Either way nothing further in the file is readable.
    while reader.read_exact(&mut buf).is_ok() {
        // Format-version gate — the SAME `!=` test as `SealSpillWriter::read_all`.
        //
        // ⚠ CORRECTED 2026-09-22. This gate refused only `buf[7] == 0`, the
        // version-0 era. `SealSpillWriter::read_all` refuses everything that is
        // not the live version, and a comment further down this file claimed
        // that was what protected the boot drain — but `read_all` has no
        // production caller. The boot drain reads staged files HERE, so a
        // version-3 record (the 24-frame ordinal space, before 2026-09-19)
        // passed straight through: ordinal 5 was `S1` there and is `M30` now,
        // so a one-second bar re-ingested as a thirty-minute bar, silently.
        //
        // `!=` rather than `<`: a record from a NEWER build is equally
        // unreadable, and the case that produces one is a deploy rollback. A
        // refused record is counted as undecodable and the file's bytes are
        // kept in `archive/` — nothing is destroyed, only kept out of a
        // timeframe it never belonged to.
        //
        // 2026-10-01 (audit PR41c): the gate is the readable RANGE
        // (`seal_spill_version_is_readable`), and a version-5 record must also
        // match its checksum. `decode_spill_record` applies both, the same way
        // for every spill reader.
        match decode_spill_record(&buf) {
            SpillRecordRead::Seal(seal) => out.push(seal),
            SpillRecordRead::OtherVersion => undecodable += 1,
            SpillRecordRead::ChecksumMismatch => {
                undecodable += 1;
                checksum_refused += 1;
            }
        }
    }
    note_checksum_refused(path, checksum_refused);
    Some((out, undecodable))
}

/// One coded line per file when records were refused for a checksum that does
/// not match their bytes (audit PR41c): each is a candle damaged on disk and
/// not replayed. Its bytes stay in the file, which is archived, not deleted.
fn note_checksum_refused(path: &Path, checksum_refused: usize) {
    if checksum_refused > 0 {
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            ?path,
            checksum_refused,
            "seal recovery: spilled records refused because their checksum does not match \
             their bytes (damaged on disk); each is one candle not replayed"
        );
    }
}

/// Decodes every NDJSON line in a staged DLQ file.
/// Returns `(records, undecodable_count)`.
fn read_staged_dlq(path: &Path) -> Option<(Vec<SerializedSeal>, usize)> {
    let Ok(file) = std::fs::File::open(path) else {
        warn!(?path, "cannot open staged dlq file");
        return None;
    };
    let mut out = Vec::new();
    let mut undecodable = 0usize;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            undecodable += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SealDlqRecord>(&line) {
            // The DLQ carried no format version until 2026-09-22, so a line
            // from the 24-frame era decoded into the nine-frame ordinal space
            // with no warning. `format_version` defaults to 0 on those lines,
            // below every real version, so they are refused here — counted,
            // and retained on disk in `archive/`.
            // 2026-10-01: a version-4 line is still read (no ordinal moved).
            Ok(record) if !seal_spill_version_is_readable(record.format_version) => {
                undecodable += 1;
            }
            Ok(record) => out.push(SerializedSeal::from(&record)),
            Err(_) => undecodable += 1,
        }
    }
    Some((out, undecodable))
}

/// Moves a confirmed file from `replaying/` to `archive/`.
fn archive_staged(path: &Path) -> std::io::Result<()> {
    let Some(replaying_dir) = path.parent() else {
        return Ok(());
    };
    let Some(root) = replaying_dir.parent() else {
        return Ok(());
    };
    let archive = root.join(SEAL_ARCHIVE_SUBDIR);
    std::fs::create_dir_all(&archive)?;
    let Some(name) = path.file_name() else {
        return Ok(());
    };
    std::fs::rename(path, free_path(&archive, name))
}

/// The narrow slice of [`ShadowCandleWriter`] the recovery drain needs.
///
/// Exists so the recovery path is testable end-to-end WITHOUT a live
/// QuestDB: `ShadowCandleWriter::for_test()` is permanently disconnected, so
/// its `flush()` can only ever fail, which would leave the success path — the
/// half that actually proves seals are re-ingested and files archived —
/// untestable and therefore unproven.
///
/// Cold path, static dispatch (`impl Trait`), no `dyn`, no allocation.
pub trait SealSink {
    /// Append one seal to the wire buffer.
    fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()>;
    /// Flush the wire buffer to the database.
    fn flush(&mut self) -> anyhow::Result<()>;
    /// Discard the retained buffer after a failed flush (poison recovery).
    fn discard_pending(&mut self);
}

impl SealSink for ShadowCandleWriter {
    fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
        Self::append_seal(self, seal)
    }
    fn flush(&mut self) -> anyhow::Result<()> {
        Self::flush(self)
    }
    fn discard_pending(&mut self) {
        Self::discard_pending(self);
    }
}

/// How long a staged recovery file whose seals the writer REFUSED stays staged
/// for another boot's retry (audit PR31a, 2026-09-27). Two days covers the next
/// trading morning's boot. Past it the refusal is in the data rather than the
/// moment, and re-reading the file every boot would re-send its good seals over
/// any newer bar with the same key.
pub const SEAL_REFUSED_FILE_RETRY_SECS: u64 = 2 * 24 * 60 * 60;

/// `true` when a staged file was last written more than
/// [`SEAL_REFUSED_FILE_RETRY_SECS`] ago. An unreadable time reads as young, so
/// the file is kept rather than archived on a guess.
fn staged_file_is_past_refusal_retry(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|written| std::time::SystemTime::now().duration_since(written).ok())
        .is_some_and(|age| age.as_secs() > SEAL_REFUSED_FILE_RETRY_SECS)
}

/// S1 (2026-10-02): the summary of committed copies a boot drain leaves when
/// it stops with files still staged, in the spill root beside `archive/`.
/// Not a `.bin`, so neither the spill retention sweep nor the cold upload
/// ever takes it for a seal file; the boot drain owns it alone.
pub const SEAL_BOOT_SUMMARY_FILE: &str = "boot-committed.summary";

/// Temporary name the summary is written under before the rename.
const SEAL_BOOT_SUMMARY_TMP: &str = "boot-committed.summary.tmp";

/// Seeds `written` from the summary a stopped drain left, if any. A missing
/// file is the normal case. An unreadable or damaged one is refused whole,
/// logged and counted: this drain then runs without that guard, exactly as
/// every drain ran before S1, and never stops for it.
///
/// # Complexity
/// O(bars in the file). Boot drain, once, cold.
fn seed_from_boot_summary(
    spill_dir: &Path,
    written: &mut BootWritten,
    outcome: &mut BootDrainOutcome,
) {
    let path = spill_dir.join(SEAL_BOOT_SUMMARY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            outcome.summary_refused += 1;
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                ?err,
                "seal recovery: the summary of copies an earlier boot committed cannot be \
                 read — this drain may write an older copy of a bar that boot committed"
            );
            return;
        }
    };
    match written.seed_from_bytes(&bytes) {
        Ok(seeded) => {
            outcome.summary_seeded += seeded.seeded;
            outcome.summary_refused += seeded.untracked;
            if seeded.untracked > 0 {
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    untracked = seeded.untracked,
                    "seal recovery: the earlier boot's summary holds more bars than this \
                     drain can track — an older copy of an untracked bar may be written"
                );
            }
            info!(
                seeded = seeded.seeded,
                "seal recovery: seeded the copies an earlier, stopped drain committed"
            );
        }
        Err(reason) => {
            outcome.summary_refused += 1;
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                ?reason,
                "seal recovery: the summary of copies an earlier boot committed is damaged \
                 and refused whole — this drain may write an older copy of a bar that boot \
                 committed"
            );
        }
    }
}

/// Writes the committed half of `written` to the summary (temp file, sync,
/// rename). A failure keeps any previous summary, which is a subset of this
/// one (it was seeded), and counts every committed bar it could not add.
///
/// # Complexity
/// O(bars) plus one file write. Boot drain, once, only when files stay staged.
fn persist_boot_summary(spill_dir: &Path, written: &BootWritten, outcome: &mut BootDrainOutcome) {
    let committed = written.committed_len();
    if committed == 0 {
        // Nothing committed and nothing seeded: there is nothing to guard.
        remove_boot_summary(spill_dir);
        return;
    }
    let (bytes, dropped) = written.encode_committed(SEAL_BOOT_WRITTEN_CAPACITY);
    if dropped > 0 {
        outcome.summary_not_persisted += dropped;
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            dropped,
            bound = SEAL_BOOT_WRITTEN_CAPACITY,
            "seal recovery: more committed bars than the summary bound — the next boot \
             may write an older copy of the bars left out"
        );
    }
    let path = spill_dir.join(SEAL_BOOT_SUMMARY_FILE);
    let tmp = spill_dir.join(SEAL_BOOT_SUMMARY_TMP);
    let result = std::fs::create_dir_all(spill_dir)
        .and_then(|()| crate::wal_applied_watermark::write_fresh(&tmp, &bytes))
        .and_then(|()| std::fs::rename(&tmp, &path));
    match result {
        Ok(()) => {
            // The rename is durable once the directory is: best effort.
            drop(std::fs::File::open(spill_dir).and_then(|dir| dir.sync_all()));
            info!(
                bars = committed - dropped,
                "seal recovery: summary of committed copies kept for the next boot"
            );
        }
        Err(err) => {
            outcome.summary_not_persisted += committed - dropped;
            drop(std::fs::remove_file(&tmp));
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                ?err,
                bars = committed - dropped,
                "seal recovery: the summary of committed copies could not be written — the \
                 next boot may write an older copy of a bar this boot committed"
            );
        }
    }
}

/// Removes the summary once no staged file is left to compare against.
fn remove_boot_summary(spill_dir: &Path) {
    let path = spill_dir.join(SEAL_BOOT_SUMMARY_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => info!("seal recovery: nothing left staged — summary of committed copies removed"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            ?path,
            ?err,
            "seal recovery: the stale summary of committed copies could not be removed"
        ),
    }
}

/// Boot-time recovery: reads every orphaned spill / DLQ file back and
/// re-ingests it into QuestDB through `writer`.
///
/// Cold path — runs ONCE at writer-loop startup, before the drain ticker.
/// Allowed to allocate.
///
/// Stops at the first flush failure (QuestDB is down; the remaining files
/// stay staged for the next boot) and reports exactly how many seals are
/// still on disk rather than in the database.
pub fn drain_recovered_seals<S: SealSink>(
    writer: &mut S,
    spill_dir: &Path,
    dlq_dir: &Path,
    max_batch: usize,
) -> BootDrainOutcome {
    let outcome = drain_recovered_seals_once(writer, spill_dir, dlq_dir, max_batch);
    // PR40b-f: the retention sweep may now delete aged unreplayed files here.
    // Whatever the drain found, it has read the folder; a file it left
    // staged stays inside the age window or is reported when it ages out.
    crate::seal_spill::note_boot_drain_ran(spill_dir);
    outcome
}

/// One boot drain; see [`drain_recovered_seals`].
fn drain_recovered_seals_once<S: SealSink>(
    writer: &mut S,
    spill_dir: &Path,
    dlq_dir: &Path,
    max_batch: usize,
) -> BootDrainOutcome {
    let mut outcome = BootDrainOutcome::default();
    let batch = max_batch.max(1);

    let mut staged = stage_pending_files(spill_dir);
    if dlq_dir != spill_dir {
        staged.extend(stage_pending_files(dlq_dir));
    }
    outcome.files_staged = staged.len();
    if staged.is_empty() {
        // Nothing staged: no file is left that could hold an older copy.
        remove_boot_summary(spill_dir);
        return outcome;
    }

    info!(
        files = staged.len(),
        "seal recovery: replaying orphaned spill/DLQ files from disk"
    );

    // Audit PR41a, Z6: the fullest copy of each bar (slot and bucket) written
    // so far in this drain, so the result does not depend on the order the
    // spill and dead-letter files are read in. Grown on demand, once per
    // boot, only when files are staged; past its cap a copy is written
    // untracked and counted, never dropped.
    let mut written = BootWritten::new(SEAL_BOOT_WRITTEN_CAPACITY);
    // S1 (2026-10-02): the record above used to live for one drain only. A
    // drain that committed a fuller copy, archived its file and then stopped
    // on a dead database left a file holding an OLDER copy staged, and the
    // next boot wrote it over the fuller row. The stopped drain now leaves a
    // summary of what it committed; seed from it before reading any file.
    seed_from_boot_summary(spill_dir, &mut written, &mut outcome);
    let mut halted = false;
    for path in &staged {
        if halted {
            // QuestDB is down — count the untouched remainder honestly
            // instead of burning a bounded flush timeout per file.
            outcome.files_left_pending += 1;
            continue;
        }
        // A file we could not READ is never a file we may ARCHIVE.
        //
        // The defect this closes (found 2026-08-21): all three unreadable
        // paths — spill open failure, dlq open failure, and an unrecognised
        // filename — returned an empty record vec, which is byte-identical to
        // "read successfully, contained nothing". `file_ok` then stayed true,
        // the record loop did not run, and `archive_staged` renamed the file
        // out of `replaying/` into `archive/`. Boot counted `files_archived`
        // and printed its success line. Those seals never reached QuestDB and
        // the next boot could not retry them, because the file was no longer
        // staged. The unrecognised-filename arm did not even log.
        //
        // Leaving it staged means a permanently unreadable file is reported on
        // every boot. That is the correct trade: loud forever beats silent
        // once, and an operator can inspect and remove a named file, whereas
        // nobody can recover a seal that was archived as if it had landed.
        let Some((records, undecodable)) = (match staged_kind(path) {
            Some(StagedKind::Spill) => read_staged_spill(path),
            Some(StagedKind::Dlq) => read_staged_dlq(path),
            None => None,
        }) else {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                "seal recovery: staged file could not be read or classified — it is \
                 NOT archived and stays staged for the next boot. Every seal in it is \
                 unrecovered. Inspect the file: an unreadable one needs its permissions \
                 or disk checked, and a file with an unrecognised name does not belong \
                 in the staging directory at all."
            );
            outcome.files_left_pending += 1;
            continue;
        };
        outcome.records_undecodable += undecodable;
        outcome.seals_recovered += records.len();
        if undecodable > 0 {
            warn!(
                ?path,
                undecodable,
                "seal recovery: records could not be decoded — bytes retained in archive/"
            );
        }

        let mut committed = 0usize;
        let mut file_ok = true;
        // Seals of this file the writer refused. Until 2026-09-27 the refusal
        // was logged and skipped, the file was archived as if every seal had
        // landed, and the boot printed "every recovered seal re-ingested".
        let mut append_failed = 0usize;
        for chunk in records.chunks(batch) {
            let mut appended = 0usize;
            for record in chunk {
                // Audit PR41a, Z6: a fuller copy of this bar was already
                // written in this drain (a dead-letter file can hold an older
                // copy than a spill file read before it, with other buckets of
                // the same slot in between). Writing this one would replace
                // the fuller row.
                if written.is_older(record) {
                    outcome.seals_superseded += 1;
                    continue;
                }
                let Some(seal) = record.try_into_buffered_seal() else {
                    // Forward-compat guard: unknown tf ordinal.
                    outcome.records_undecodable += 1;
                    outcome.seals_recovered = outcome.seals_recovered.saturating_sub(1);
                    continue;
                };
                // 2026-09-19 — the RETIRED-FRAME GATE that stood here is GONE, and
                // the guarantee it provided is now carried by the format version.
                // It refused a seal whose timeframe no longer emitted, so a file
                // written when `candles_10s` / `_15s` / `_30s` / `_2m` still existed
                // could not re-ingest into a table this boot had just dropped. Two
                // things retired it together: `SEAL_SPILL_FORMAT_VERSION` moved to 4
                // when the nine-frame collapse RENUMBERED the ordinals, and the
                // version gates in `read_staged_spill` / `read_staged_dlq` refuse
                // every record not written under it — so no record from the
                // 24-frame ordinal space reaches this loop. And every ordinal the
                // nine-frame table can decode IS one of the operator's nine, so the
                // predicate was tautologically true. A gate that can only return
                // true reads as a live filter to the next author; the version
                // refusal is the real one.
                //
                // ⚠ CORRECTED 2026-09-22 — until today this paragraph named
                // `seal_spill::read_all` as that refusal. It is not: `read_all`
                // has no production caller, and the two readers this loop DOES
                // use refused only version 0 (spill) and nothing at all (DLQ).
                // So the claim this gate was removed on was false for three days.
                // Both are now `!=` the live version, pinned by
                // `boot_drain_refuses_a_spill_record_from_another_format_version`
                // and `boot_drain_refuses_an_unversioned_dlq_line`.
                if let Err(append_err) = writer.append_seal(&seal) {
                    error!(
                        code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                        ?append_err,
                        security_id = seal.security_id,
                        "seal recovery: append failed for a recovered seal — its file \
                         stays staged for the next boot"
                    );
                    append_failed += 1;
                    continue;
                }
                if !written.record(record) {
                    outcome.seals_untracked += 1;
                }
                appended += 1;
            }
            if appended == 0 {
                continue;
            }
            match writer.flush() {
                Ok(()) => {
                    committed += appended;
                    written.commit_pending();
                }
                Err(flush_err) => {
                    error!(
                        code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                        ?flush_err,
                        ?path,
                        pending = records.len().saturating_sub(committed),
                        "seal recovery: flush failed — file stays staged for the next boot"
                    );
                    // Poison-buffer recovery, same reasoning as `drain_once`.
                    writer.discard_pending();
                    // Not committed: never persisted as a fuller copy.
                    written.discard_pending();
                    file_ok = false;
                    break;
                }
            }
        }

        outcome.seals_reingested += committed;
        outcome.seals_append_failed += append_failed;

        if file_ok && append_failed > 0 && !staged_file_is_past_refusal_retry(path) {
            // The database is up (every flush landed), so the remaining files
            // are still worth trying: no `halted`. Re-reading this file on the
            // next boot re-sends the seals that did land, and the DEDUP key
            // collapses them.
            outcome.files_left_pending += 1;
            outcome.seals_left_pending += append_failed;
        } else if file_ok {
            if append_failed > 0 {
                // Bounded retry: a file older than the retry window has been
                // refused on every boot since, so the refusal is in the data,
                // not the moment. Archive it (its bytes stay in archive/) rather
                // than re-send its good seals over newer bars every morning.
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    ?path,
                    append_failed,
                    retry_secs = SEAL_REFUSED_FILE_RETRY_SECS,
                    "seal recovery: seals in this file were refused on every boot for the \
                     whole retry window — archived NOT re-ingested; the bytes stay in archive/"
                );
            }
            match archive_staged(path) {
                Ok(()) => outcome.files_archived += 1,
                Err(err) => {
                    // Re-ingest succeeded but the rename did not. The file
                    // stays staged; the next boot re-reads it and QuestDB's
                    // DEDUP keys collapse the duplicate rows.
                    warn!(?path, ?err, "seal recovery: archive rename failed");
                    outcome.files_left_pending += 1;
                }
            }
        } else {
            outcome.files_left_pending += 1;
            outcome.seals_left_pending += records.len().saturating_sub(committed);
            halted = true;
        }
    }

    // S1: a drain that leaves files staged hands what it (and every earlier
    // stopped drain) committed to the next boot; one that leaves none removes
    // the summary, since no staged copy is left to compare.
    if outcome.files_left_pending > 0 {
        persist_boot_summary(spill_dir, &written, &mut outcome);
    } else {
        remove_boot_summary(spill_dir);
    }

    if outcome.files_left_pending > 0 {
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            files_pending = outcome.files_left_pending,
            seals_pending = outcome.seals_left_pending,
            seals_append_failed = outcome.seals_append_failed,
            seals_reingested = outcome.seals_reingested,
            "seal recovery INCOMPLETE — sealed candles are on DISK and NOT in QuestDB"
        );
    } else if outcome.seals_append_failed > 0 {
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            seals_append_failed = outcome.seals_append_failed,
            seals_reingested = outcome.seals_reingested,
            files_archived = outcome.files_archived,
            "seal recovery finished with refused seals archived NOT re-ingested — \
             their bytes are in archive/"
        );
    } else {
        info!(
            files_archived = outcome.files_archived,
            seals_reingested = outcome.seals_reingested,
            records_undecodable = outcome.records_undecodable,
            "seal recovery complete — every recovered seal re-ingested"
        );
    }

    // Boot-only, so never on a hot path. A skipped record is NOT a re-ingested
    // one, and until 2026-09-22 the only summary of it was a field on the
    // `info!` success line above — so a boot that refused every record of a
    // pre-v4 spill file read as a clean recovery. Name it at `warn!` level,
    // with the module's own error code, so the skip is findable.
    if outcome.records_undecodable > 0 {
        warn!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            records_undecodable = outcome.records_undecodable,
            current_format_version = SEAL_SPILL_FORMAT_VERSION,
            "seal recovery SKIPPED {} record(s) it could not decode — most often \
             files written by an older build in a different format. Those seals \
             were NOT re-ingested into QuestDB; their files stay on disk (archive/ \
             or replaying/) for inspection — this drain deletes nothing, and the \
             spill retention sweep removes them once they are older than its window. \
             The page for them is the AGGREGATOR-DROP-01 line with \
             source=seal_unrecovered",
            outcome.records_undecodable
        );
    }

    outcome
}

// ---------------------------------------------------------------------------
// Mid-session spill replay (audit PR15, 2026-09-27)
// ---------------------------------------------------------------------------
//
// ## The gap this closes
//
// The boot drain above is the only reader of the seal spill. A seal rescued to
// disk at 10:05 therefore stayed out of `candles_*` until the NEXT boot, which
// on this box is the next trading morning: a whole session of candles missing
// from the database while the process that held them kept running. Measured
// 2026-08-20: `spilled: 541,519` in one session.
//
// ## The contract
//
// * **Health gate.** Nothing replays until the live writer has flushed
//   successfully and has not failed a flush for
//   [`SEAL_REPLAY_HEALTHY_SECS`]. A failed flush — live or replay — resets the
//   gate, so a flapping database is never fed a backlog.
// * **Live first.** A step runs only when the live drain left the ring empty.
//   The step itself runs on the writer's own cycle, so a live seal that
//   arrives during a replay flush waits in the channel for at most that one
//   flush: the replay delays the next live cycle by one bounded step, and never
//   takes a live seal's place.
// * **Rate cap.** At most [`SEAL_REPLAY_SEALS_PER_CYCLE`] seals per writer
//   cycle (100 ms): ~5,000 seals a second, 541,519 in about two minutes. The
//   figure is deliberately half the writer's measured capacity because the
//   database's WAL apply lag was still growing on 2026-09-08 (Assumed to hold
//   today; not re-measured).
// * **One bad record cannot stall the file.** A replay flush that fails halves
//   the next step (512 -> 256 -> ... -> 1) and a success doubles it back.
//   Two failures in a row at a step of ONE record, at the same position, skip
//   that record: it is counted as skipped, its bytes survive in `archive/`,
//   and an `error!` names it. Each failure also resets the health gate, so
//   isolating a poison record takes at least ten healthy minutes; a
//   database that is merely down never reaches a step of one.
// * **No loss while appending.** The live file is taken under the spill
//   writer's own append lock ([`SealSpillWriter::with_appends_paused`]), so a
//   seal appended at the same instant lands either in the staged file or in a
//   fresh one, never in a moved inode nothing reads.
// * **Same staging as boot.** Files move to `replaying/` and, once every
//   record is committed, to `archive/`. A crash mid-file leaves it in
//   `replaying/`, where the next boot (or this replay) reads it again, and the
//   candle tables' DEDUP keys `(ts, security_id, segment, feed)` collapse the
//   rows already written.
// * **A failed replay flush never re-spills.** The batch is discarded and the
//   file keeps its position, exactly as the boot drain does and for the same
//   reason: re-spilling a replayed seal would multiply it on disk.
//
// ## Honest limits
//
// * **Last write wins, so the spill ends on the fullest copy (audit PR41a).**
//   A replayed seal UPSERTs over whatever row the tables hold for its key. A
//   bar can be written more than once (a late tick amends the bar that just
//   sealed; the day-close seal can add carried volume), so until 2026-10-01 a
//   spilled original replayed over an amended copy written live meanwhile.
//   Now the spill writer keeps a ledger of the newest spilled bucket of every
//   slot (`seal_spill_ledger`): a fuller copy committed live is appended to
//   the spill behind the original, this replay drops a record the spill holds
//   a fuller copy of, and the boot drain never writes a copy less full than
//   one it already wrote. Counted on `tv_seal_spill_superseded_total{kind}`
//   and `tv_seal_writer_drain_total{kind="boot_superseded"}`.
//   What is NOT covered: a crash between a live flush and its mirror append
//   (one drain cycle), and a mirror append that fails (counted as
//   `mirror_failed`, coded `error!`). In both, the next replay can still write
//   the older copy over the stored row. The boot drain does not read the
//   stored row to compare, because a database read per recovered seal is the
//   cost the spill exists to avoid.
// * **Clock steps.** A wall clock that steps backwards restarts the gate
//   (never opens it early), and makes a directory scan due at once.
//
// ## The database's word, not only the live writer's (audit PR41b)
//
// * **A health check reopens the gate.** The gate used to open only on clean
//   LIVE flushes, so a spill made after the last live write (the last flush
//   before the close failed) waited for the next boot. It now also opens
//   once the QuestDB WAL-suspension watcher has reported a clean probe after
//   the last failure and [`SEAL_REPLAY_HEALTHY_SECS`] have passed since it.
// * **A suspect table closes the gate and keeps the file.** An ILP `2xx` from
//   a WAL-suspended table is acknowledged and never applied (see
//   `wal_applied_watermark`). While the watcher reports a table suspended,
//   lagging, or itself blind, nothing replays. A file read to its end is not
//   archived until two more clean probes have reported, so one whole probe
//   ran after its last write. If suspicion begins first, the file is read
//   again from its start, and so is the file being read: the DEDUP keys
//   collapse what had landed. With no watcher running (no probe ever seen),
//   a finished file is archived at once, as before.
// * **A flapping database is not a bad record.** A failed flush whose error is
//   the transport, the server's configuration, or the candle tables not yet
//   keyed says nothing about the record and never counts toward skipping it.
//   Any other failure at a step of one record counts as a strike, the first
//   one always and each later one only when the database accepted a write in
//   between (a clean live flush, or a clean probe, since the last failure).
//   Only [`SEAL_REPLAY_POISON_FAILURES`] strikes skip the record.
// * **One stuck file never holds the rest.** After
//   [`SEAL_REPLAY_STUCK_FAILURES`] failed flushes at one position with no
//   progress, the file is parked where it is for [`SEAL_REPLAY_PARK_SECS`] and
//   the replay moves to the next staged file. A parked file resumes at the
//   same position. Later files can then reach the database before an older
//   one; an older copy of a bucket the spill also holds newer is still dropped
//   by the PR41a ledger (a key the ledger could not track is not).
//
// ## The dead-letter file replays at boot only (audit row 36)
//
// Only the binary spill is replayed here. The NDJSON DLQ is the last-resort
// tier: a seal reaches it only when the spill append itself failed, which
// means the volume under the spill is failing. It also has no paused-append
// staging (`SealSpillWriter::with_appends_paused` has no DLQ counterpart), so
// moving its file mid-session could lose a seal appended at the same instant.
// So the DLQ stays with the boot drain: its seals are held on disk, not lost,
// and reach the candle tables on the next boot. Recorded in the plan as the
// decision for row 36.

/// How long the live writer must flush cleanly before a replay step may run.
pub const SEAL_REPLAY_HEALTHY_SECS: i64 = 60;

/// Most spilled seals re-ingested per writer cycle (every 100 ms).
pub const SEAL_REPLAY_SEALS_PER_CYCLE: usize = 512;

/// Strikes against one record, at a step of one record and the same position,
/// before it is skipped as poison. A strike is a failure that was not the
/// transport (audit PR41b); after the first, only a failure that follows a
/// write the database accepted counts.
const SEAL_REPLAY_POISON_FAILURES: u32 = 2;

/// How often, while idle and healthy, the replay looks for spill files.
pub const SEAL_REPLAY_SCAN_EVERY_SECS: i64 = 30;

/// Files the replay gave up on for this process (unreadable, or the archive
/// rename failed). Bounded so a directory of broken files cannot grow it; past
/// the bound the replay stops looking for new files until the next boot.
const SEAL_REPLAY_SKIP_LIMIT: usize = 64;

/// Failed flushes at one position, with no progress, before the file is
/// parked and the replay moves on (audit PR41b). Nine halvings take a step
/// from [`SEAL_REPLAY_SEALS_PER_CYCLE`] down to one record, so twelve leaves
/// the poison isolation three attempts at a step of one before parking.
pub const SEAL_REPLAY_STUCK_FAILURES: u32 = 12;

/// How long a parked file waits before the replay resumes it (audit PR41b).
pub const SEAL_REPLAY_PARK_SECS: i64 = 600;

/// Most files parked, or read to their end and waiting for the database to
/// confirm them, at once. Same bound as [`SEAL_REPLAY_SKIP_LIMIT`]; a full list
/// keeps the file where it is rather than dropping it.
const SEAL_REPLAY_HELD_LIMIT: usize = SEAL_REPLAY_SKIP_LIMIT;

/// Clean probes that must report after a file's last write before it is
/// archived. Two, so one whole probe started after that write.
const SEAL_REPLAY_ARCHIVE_CONFIRM_PROBES: u64 = 2;

/// What the QuestDB WAL-suspension watcher last reported, as the replay reads
/// it (audit PR41b).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayProbe {
    /// Clean probes reported so far this process. `0` with `suspect == false`
    /// means no watcher has reported, and the replay behaves as before PR41b.
    pub clean_probes: u64,
    /// `true` while a table is suspended, lagging, or the probe is blind.
    pub suspect: bool,
}

impl ReplayProbe {
    /// The process-global watcher's current view. Two atomic loads.
    #[must_use]
    pub fn current() -> Self {
        let wm = crate::wal_applied_watermark::applied_watermark();
        Self {
            clean_probes: wm.clean_probe_count(),
            suspect: wm.is_sink_suspect(),
        }
    }

    /// `true` once any probe has reported, clean or not.
    const fn is_running(self) -> bool {
        self.clean_probes > 0 || self.suspect
    }
}

/// Why a replay flush failed, as far as it bears on the record (audit PR41b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayFlushFailure {
    /// The transport, the server's configuration or authentication, or the
    /// candle tables not yet keyed. Says nothing about the record.
    Environment,
    /// The database answered and refused the batch, or the cause is unknown.
    Refused,
}

/// Classify a replay flush failure. O(chain length), cold.
fn classify_replay_flush_failure(err: &anyhow::Error) -> ReplayFlushFailure {
    if err
        .downcast_ref::<crate::shadow_candle_writer::CandleTablesNotKeyed>()
        .is_some()
    {
        return ReplayFlushFailure::Environment;
    }
    match err
        .downcast_ref::<questdb::Error>()
        .map(questdb::Error::code)
    {
        Some(
            questdb::ErrorCode::SocketError
            | questdb::ErrorCode::CouldNotResolveAddr
            | questdb::ErrorCode::AuthError
            | questdb::ErrorCode::TlsError
            | questdb::ErrorCode::HttpNotSupported
            | questdb::ErrorCode::ConfigError,
        ) => ReplayFlushFailure::Environment,
        _ => ReplayFlushFailure::Refused,
    }
}

/// What one [`MidSessionReplay::step`] did. Every field maps to a
/// `tv_seal_replay_total{kind=...}` label emitted by the writer loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Live spill files moved into `replaying/` this step.
    pub files_staged: usize,
    /// Spilled seals a successful flush committed to QuestDB this step.
    pub seals_reingested: usize,
    /// Records that could not be decoded or appended. Their bytes survive in
    /// `archive/`.
    pub records_skipped: usize,
    /// Files fully replayed and moved to `archive/` this step.
    pub files_archived: usize,
    /// `true` when this step's flush failed. The file keeps its position and
    /// the health gate resets.
    pub flush_failed: bool,
    /// Files parked after [`SEAL_REPLAY_STUCK_FAILURES`] failed flushes at one
    /// position, so the next file could go ahead (audit PR41b).
    pub files_parked: usize,
    /// Files to be read again from their start because the database became
    /// suspect before it confirmed them (audit PR41b). Set by
    /// [`MidSessionReplay::observe_probe`]; the runner folds it in.
    pub files_rewound: usize,
}

impl ReplayOutcome {
    /// `true` if the step did nothing at all.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.files_staged == 0
            && self.seals_reingested == 0
            && self.records_skipped == 0
            && self.files_archived == 0
            && !self.flush_failed
            && self.files_parked == 0
            && self.files_rewound == 0
    }
}

/// The file being replayed and how far into it the database has confirmed.
#[derive(Debug)]
struct ReplayCursor {
    path: PathBuf,
    /// Byte offset of the first record not yet committed. Always a multiple
    /// of [`SEAL_SPILL_RECORD_SIZE`].
    offset: u64,
    /// Records read per step, 1..=[`SEAL_REPLAY_SEALS_PER_CYCLE`]. Halved by
    /// a failed flush, doubled by a clean one.
    step_records: usize,
    /// Strikes against the record at `offset`, at a step of one record. See
    /// [`SEAL_REPLAY_POISON_FAILURES`].
    single_record_failures: u32,
    /// Failed flushes at `offset` since it last moved, of any kind and any
    /// step size. See [`SEAL_REPLAY_STUCK_FAILURES`].
    failures_at_offset: u32,
    /// `(clean live flushes, clean probes)` seen at the last failed flush.
    /// Either moving on since means the database accepted something in
    /// between.
    evidence_mark: (u64, u64),
}

impl ReplayCursor {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            step_records: SEAL_REPLAY_SEALS_PER_CYCLE,
            single_record_failures: 0,
            failures_at_offset: 0,
            evidence_mark: (0, 0),
        }
    }

    /// Start the file again from its first record.
    fn rewind(&mut self) {
        self.offset = 0;
        self.step_records = SEAL_REPLAY_SEALS_PER_CYCLE;
        self.single_record_failures = 0;
        self.failures_at_offset = 0;
    }
}

/// A file set aside after it stopped making progress. Resumes at its cursor.
#[derive(Debug)]
struct ParkedFile {
    cursor: ReplayCursor,
    /// Wall-clock second from which the replay may resume it.
    retry_at: i64,
}

/// A file read to its end, waiting for the database to confirm it.
#[derive(Debug)]
struct AwaitingArchive {
    path: PathBuf,
    /// Clean probes reported when its last record was flushed.
    clean_probes_at_finish: u64,
}

/// Mid-session replay of the seal spill. Owned by the seal writer runner and
/// stepped once per writer cycle, after the live drain.
///
/// Cold path: runs on the writer loop inside `block_in_place`, never on the
/// frame drain. Each step opens the staged file, reads at most
/// [`SEAL_REPLAY_SEALS_PER_CYCLE`] records into a reused buffer and flushes
/// them once: O(records) per step, bounded by the cap. The parked and
/// awaiting lists hold at most [`SEAL_REPLAY_HELD_LIMIT`] files each and are
/// scanned linearly once per step at most.
#[derive(Debug, Default)]
pub struct MidSessionReplay {
    /// Wall-clock second of the first clean live flush since the last failure.
    healthy_since: Option<i64>,
    /// Wall-clock second of the last failed flush, live or replay.
    last_failure_secs: Option<i64>,
    /// Clean probes reported when that failure happened.
    clean_probes_at_failure: u64,
    /// Clean live flushes seen this process.
    live_ok_flushes: u64,
    /// The watcher's view as of the last [`Self::observe_probe`].
    probe: ReplayProbe,
    /// Wall-clock second of the last directory scan.
    last_scan: Option<i64>,
    cursor: Option<ReplayCursor>,
    /// Files given up on for this process. See [`SEAL_REPLAY_SKIP_LIMIT`].
    skipped: Vec<PathBuf>,
    /// Files set aside after they stopped making progress.
    parked: Vec<ParkedFile>,
    /// Files read to their end, waiting for a clean probe behind them.
    awaiting_archive: Vec<AwaitingArchive>,
    /// Reused decode buffer, at most one step's worth of seals.
    batch: Vec<BufferedSeal>,
}

impl MidSessionReplay {
    /// Feed one live drain outcome into the health gate.
    ///
    /// A cycle that popped seals and did not flush them is a failure and
    /// resets the gate. A clean flush starts the clock if it is not running.
    /// An idle cycle says nothing about the database and changes nothing.
    pub fn observe(&mut self, drain: &DrainOutcome, now_unix_secs: i64) {
        // A clock that stepped backwards restarts the gate rather than
        // leaving it shut until the clock catches up (or opening it early).
        if self
            .healthy_since
            .is_some_and(|since| now_unix_secs < since)
        {
            self.healthy_since = Some(now_unix_secs);
        }
        if drain.ring_seals_popped > 0 && !drain.flushed_ok {
            self.note_failure(now_unix_secs);
        } else if drain.flushed_ok {
            self.live_ok_flushes = self.live_ok_flushes.saturating_add(1);
            if self.healthy_since.is_none() {
                self.healthy_since = Some(now_unix_secs);
            }
        }
    }

    /// Feed the QuestDB watcher's current view into the replay (audit PR41b).
    /// Returns how many files are to be read again from their start because
    /// the database turned suspect before it confirmed them.
    ///
    /// O([`SEAL_REPLAY_HELD_LIMIT`]) on the transition into suspicion, O(1)
    /// otherwise.
    pub fn observe_probe(&mut self, probe: ReplayProbe, now_unix_secs: i64) -> usize {
        let entering = probe.suspect && !self.probe.suspect;
        self.probe = probe;
        if !probe.suspect {
            return 0;
        }
        // Suspect: nothing replays, and the gate must be earned again.
        self.note_failure(now_unix_secs);
        if !entering {
            return 0;
        }
        // Every ack since the last clean probe may have been a lie, so what
        // was read since then is read again. The DEDUP keys collapse what did
        // land; the file is never archived on an unconfirmed read.
        let mut rewound = 0usize;
        if let Some(cursor) = self.cursor.as_mut()
            && cursor.offset > 0
        {
            cursor.rewind();
            rewound += 1;
        }
        // O(1) EXEMPT: at most SEAL_REPLAY_HELD_LIMIT entries, once per suspicion
        for parked in &mut self.parked {
            if parked.cursor.offset > 0 {
                parked.cursor.rewind();
                rewound += 1;
            }
        }
        // A file waiting for confirmation leaves the list, so the next scan
        // picks it up again and reads it from its first record.
        rewound += self.awaiting_archive.len();
        self.awaiting_archive.clear();
        if rewound > 0 {
            warn!(
                files_rewound = rewound,
                "seal replay: QuestDB reports a suspended or lagging table — files the \
                 replay wrote since the last clean check are read again from their start \
                 once it is healthy"
            );
        }
        rewound
    }

    /// Close the gate and remember when, for the probe path to reopen it.
    fn note_failure(&mut self, now_unix_secs: i64) {
        self.healthy_since = None;
        self.last_failure_secs = Some(now_unix_secs);
        self.clean_probes_at_failure = self.probe.clean_probes;
    }

    /// `true` once the database is healthy enough to take a replay step:
    /// never while the watcher reports a suspect table; otherwise once the
    /// live writer has flushed cleanly for [`SEAL_REPLAY_HEALTHY_SECS`] with no
    /// failure since, or once a clean probe has reported after the last
    /// failure and [`SEAL_REPLAY_HEALTHY_SECS`] have passed since it.
    #[must_use]
    pub fn is_healthy(&self, now_unix_secs: i64) -> bool {
        if self.probe.suspect {
            return false;
        }
        let live = self
            .healthy_since
            .is_some_and(|since| now_unix_secs.saturating_sub(since) >= SEAL_REPLAY_HEALTHY_SECS);
        let quiet_since_failure = self.last_failure_secs.is_none_or(|at| {
            now_unix_secs >= at && now_unix_secs.saturating_sub(at) >= SEAL_REPLAY_HEALTHY_SECS
        });
        let probe = self.probe.clean_probes > self.clean_probes_at_failure && quiet_since_failure;
        live || probe
    }

    /// The file currently being replayed, if any.
    #[cfg(test)]
    fn current_file(&self) -> Option<&Path> {
        self.cursor.as_ref().map(|c| c.path.as_path())
    }

    /// Run one replay step.
    ///
    /// `ring_is_empty` is whether the live drain left the ring empty this
    /// cycle; a step never runs behind live work.
    pub fn step<S: SealSink>(
        &mut self,
        writer: &mut S,
        spill: &crate::seal_spill::SealSpillWriter,
        spill_dir: &Path,
        ring_is_empty: bool,
        now_unix_secs: i64,
    ) -> ReplayOutcome {
        let mut outcome = ReplayOutcome::default();
        self.archive_confirmed(&mut outcome);
        if !ring_is_empty || !self.is_healthy(now_unix_secs) {
            return outcome;
        }
        if self.cursor.is_none() {
            // A clock that stepped backwards makes the scan due at once.
            let due = self.last_scan.is_none_or(|at| {
                now_unix_secs < at
                    || now_unix_secs.saturating_sub(at) >= SEAL_REPLAY_SCAN_EVERY_SECS
            });
            if !due || self.skipped.len() >= SEAL_REPLAY_SKIP_LIMIT {
                return outcome;
            }
            self.last_scan = Some(now_unix_secs);
            outcome.files_staged = stage_live_spill_files(spill, spill_dir);
            let Some(path) = self.next_staged_file(spill_dir, now_unix_secs) else {
                // Z6: every staged file is consumed. With nothing parked,
                // waiting for confirmation or given up on, every copy appended
                // up to the last clean staging has been replayed, so the spill
                // ledger can forget the bars that can no longer be written.
                if self.parked.is_empty()
                    && self.awaiting_archive.is_empty()
                    && self.skipped.is_empty()
                {
                    let forgotten = spill.forget_replayed();
                    if forgotten > 0 {
                        tracing::debug!(
                            forgotten,
                            "seal replay: spill ledger forgot bars whose copies are all replayed"
                        );
                    }
                }
                return outcome;
            };
            // A parked file resumes where it stopped.
            let cursor = match self.parked.iter().position(|p| p.cursor.path == path) {
                Some(at) => {
                    let mut cursor = self.parked.swap_remove(at).cursor;
                    cursor.failures_at_offset = 0;
                    info!(
                        ?path,
                        offset = cursor.offset,
                        "seal replay: resuming a parked spill file"
                    );
                    cursor
                }
                None => {
                    info!(
                        ?path,
                        files_staged = outcome.files_staged,
                        "seal replay: re-ingesting a spill file mid-session"
                    );
                    ReplayCursor::new(path)
                }
            };
            self.cursor = Some(cursor);
        }
        self.advance(writer, spill, now_unix_secs, &mut outcome);
        outcome
    }

    /// Archive every file the database has confirmed: two clean probes have
    /// reported since its last record was flushed, and none is suspect now.
    fn archive_confirmed(&mut self, outcome: &mut ReplayOutcome) {
        if self.probe.suspect || self.awaiting_archive.is_empty() {
            return;
        }
        let clean_probes = self.probe.clean_probes;
        let mut i = 0usize;
        // O(1) EXEMPT: at most SEAL_REPLAY_HELD_LIMIT entries, cold writer cycle
        while i < self.awaiting_archive.len() {
            let entry = &self.awaiting_archive[i];
            let confirmed = clean_probes
                >= entry
                    .clean_probes_at_finish
                    .saturating_add(SEAL_REPLAY_ARCHIVE_CONFIRM_PROBES);
            if !confirmed {
                i += 1;
                continue;
            }
            let entry = self.awaiting_archive.swap_remove(i);
            match archive_staged(&entry.path) {
                Ok(()) => {
                    outcome.files_archived += 1;
                    info!(
                        path = ?entry.path,
                        "seal replay: spill file re-ingested, confirmed by QuestDB, archived"
                    );
                }
                Err(err) => {
                    warn!(path = ?entry.path, ?err, "seal replay: archive rename failed");
                    self.give_up_on(entry.path);
                }
            }
        }
    }

    /// Oldest staged spill file not given up on, not waiting for
    /// confirmation, and not parked (or parked and due).
    fn next_staged_file(&self, spill_dir: &Path, now_unix_secs: i64) -> Option<PathBuf> {
        let replaying = spill_dir.join(SEAL_REPLAYING_SUBDIR);
        // O(1) EXEMPT: cold directory listing, at most once per scan interval
        let mut staged: Vec<PathBuf> = std::fs::read_dir(&replaying)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_seal_file(p) && staged_kind(p) == Some(StagedKind::Spill))
            .filter(|p| !self.skipped.contains(p))
            .filter(|p| !self.awaiting_archive.iter().any(|a| &a.path == p))
            .filter(|p| {
                self.parked
                    .iter()
                    .find(|parked| &parked.cursor.path == p)
                    .is_none_or(|parked| now_unix_secs >= parked.retry_at)
            })
            .collect();
        sort_by_write_time(&mut staged);
        staged.into_iter().next()
    }

    /// Give up on the current file for this process. The next boot's drain
    /// retries it.
    fn skip_current(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            self.give_up_on(cursor.path);
        }
        // Scan again at once: another file may be waiting.
        self.last_scan = None;
    }

    fn give_up_on(&mut self, path: PathBuf) {
        self.skipped.push(path);
        if self.skipped.len() == SEAL_REPLAY_SKIP_LIMIT {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                files_skipped = SEAL_REPLAY_SKIP_LIMIT,
                "seal replay: gave up on too many spill files — the replay stops for \
                 this process and the next boot's drain retries them"
            );
        }
    }

    /// Replay up to one step's worth of records from the cursor.
    fn advance<S: SealSink>(
        &mut self,
        writer: &mut S,
        spill: &crate::seal_spill::SealSpillWriter,
        now_unix_secs: i64,
        outcome: &mut ReplayOutcome,
    ) {
        let Some(cursor) = self.cursor.as_ref() else {
            return;
        };
        let path = cursor.path.clone();
        let offset = cursor.offset;
        let step_records = cursor.step_records;
        let Some((records_read, at_eof)) = self.read_step(&path, offset, step_records, outcome)
        else {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                "seal replay: a staged spill file could not be read — left staged for the \
                 next boot's drain"
            );
            self.skip_current();
            return;
        };

        // Keep in `self.batch` only the seals handed to the writer, so a clean
        // flush can record exactly those as committed (Z6).
        let mut records_skipped = 0usize;
        self.batch.retain(|seal| {
            // Audit PR41a, Z6: a fuller copy of this bar was already committed
            // (live, or by an earlier replay step: a later file replayed
            // before this parked one resumed). Writing this one would put the
            // older copy over the stored row. Counted by the spill.
            if spill.replay_is_superseded(seal) {
                return false;
            }
            if let Err(append_err) = writer.append_seal(seal) {
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    ?append_err,
                    security_id = seal.security_id,
                    "seal replay: append failed for a spilled seal"
                );
                records_skipped += 1;
                return false;
            }
            true
        });
        outcome.records_skipped += records_skipped;
        let appended = self.batch.len();
        if appended > 0 {
            if let Err(flush_err) = writer.flush() {
                writer.discard_pending();
                outcome.flush_failed = true;
                self.on_replay_flush_failed(&path, offset, &flush_err, now_unix_secs, outcome);
                self.note_failure(now_unix_secs);
                return;
            }
            outcome.seals_reingested += appended;
            spill.note_replay_commits(&self.batch);
        }

        let consumed =
            u64::try_from(records_read.saturating_mul(SEAL_SPILL_RECORD_SIZE)).unwrap_or(u64::MAX);
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.offset = cursor.offset.saturating_add(consumed);
            cursor.single_record_failures = 0;
            cursor.failures_at_offset = 0;
            cursor.step_records = cursor
                .step_records
                .saturating_mul(2)
                .min(SEAL_REPLAY_SEALS_PER_CYCLE);
        }
        if at_eof {
            self.finish_file(path, outcome);
        }
    }

    /// The file has been read to its end. With a QuestDB watcher running it
    /// waits for two clean probes behind it; without one it is archived now.
    fn finish_file(&mut self, path: PathBuf, outcome: &mut ReplayOutcome) {
        if self.probe.is_running() {
            if self.awaiting_archive.len() >= SEAL_REPLAY_HELD_LIMIT {
                // Hold the file at its end; the next step tries again once
                // the list has drained. Nothing is lost by waiting.
                return;
            }
            self.awaiting_archive.push(AwaitingArchive {
                path,
                clean_probes_at_finish: self.probe.clean_probes,
            });
            self.cursor = None;
            self.last_scan = None;
            return;
        }
        match archive_staged(&path) {
            Ok(()) => {
                outcome.files_archived += 1;
                info!(?path, "seal replay: spill file re-ingested and archived");
                self.cursor = None;
                self.last_scan = None;
            }
            Err(err) => {
                // Committed but not archived. Replaying it again would
                // only rewrite the same rows, so leave it for the boot.
                warn!(?path, ?err, "seal replay: archive rename failed");
                self.skip_current();
            }
        }
    }

    /// A replay flush failed: the file keeps its position and the next step
    /// reads half as many records, so a single record the database refuses is
    /// isolated instead of failing every step of the file forever. At a step
    /// of one record, [`SEAL_REPLAY_POISON_FAILURES`] strikes skip it (see the
    /// strike rule in the module notes). After [`SEAL_REPLAY_STUCK_FAILURES`]
    /// failures at one position the file is parked so the next can go ahead.
    fn on_replay_flush_failed(
        &mut self,
        path: &Path,
        offset: u64,
        flush_err: &anyhow::Error,
        now_unix_secs: i64,
        outcome: &mut ReplayOutcome,
    ) {
        let evidence_now = (self.live_ok_flushes, self.probe.clean_probes);
        let failure = classify_replay_flush_failure(flush_err);
        let Some(cursor) = self.cursor.as_mut() else {
            return;
        };
        cursor.failures_at_offset = cursor.failures_at_offset.saturating_add(1);
        let database_accepted_since =
            evidence_now.0 > cursor.evidence_mark.0 || evidence_now.1 > cursor.evidence_mark.1;
        cursor.evidence_mark = evidence_now;

        if cursor.step_records > 1 {
            cursor.step_records /= 2;
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?flush_err,
                ?path,
                offset,
                ?failure,
                next_step_records = cursor.step_records,
                "seal replay: flush failed — the file keeps its place, the next step reads \
                 half as many records, and the replay waits for the database to be healthy \
                 again"
            );
        } else {
            let strike = failure == ReplayFlushFailure::Refused
                && (cursor.single_record_failures == 0 || database_accepted_since);
            if strike {
                cursor.single_record_failures = cursor.single_record_failures.saturating_add(1);
            }
            if cursor.single_record_failures >= SEAL_REPLAY_POISON_FAILURES {
                let record_bytes = u64::try_from(SEAL_SPILL_RECORD_SIZE).unwrap_or(u64::MAX);
                cursor.offset = cursor.offset.saturating_add(record_bytes);
                cursor.single_record_failures = 0;
                cursor.failures_at_offset = 0;
                outcome.records_skipped += 1;
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    ?flush_err,
                    ?path,
                    offset,
                    "seal replay: the database refused this one spilled seal in every attempt, \
                     and accepted other writes in between — skipped; its bytes stay in the \
                     file, which moves to archive/"
                );
                return;
            }
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?flush_err,
                ?path,
                offset,
                ?failure,
                strike,
                strikes = cursor.single_record_failures,
                "seal replay: flush of a single spilled seal failed — retried after the \
                 database is healthy again; a transport failure, or one with no write \
                 accepted since the last, is not held against the record"
            );
        }

        if cursor.failures_at_offset >= SEAL_REPLAY_STUCK_FAILURES
            && self.parked.len() < SEAL_REPLAY_HELD_LIMIT
            && let Some(mut cursor) = self.cursor.take()
        {
            cursor.failures_at_offset = 0;
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                offset,
                failures = SEAL_REPLAY_STUCK_FAILURES,
                retry_in_secs = SEAL_REPLAY_PARK_SECS,
                "seal replay: a spill file made no progress — parked where it stopped so the \
                 next staged file can go ahead; it resumes at the same record later"
            );
            self.parked.push(ParkedFile {
                cursor,
                retry_at: now_unix_secs.saturating_add(SEAL_REPLAY_PARK_SECS),
            });
            self.last_scan = None;
            outcome.files_parked += 1;
        }
    }

    /// Decode up to `step_records` records starting at `offset` into
    /// `self.batch`. Returns `(records consumed, reached end of file)`, or
    /// `None` when the file cannot be opened, positioned or read.
    ///
    /// A torn trailing record (fewer than 128 bytes) ends the file, exactly as
    /// in [`read_staged_spill`]: nothing is appended to a staged file, so a
    /// short tail can only be a write that never completed. Any OTHER read
    /// error is not an end of file and returns `None`, so the file is left
    /// staged for the boot drain rather than archived half-read.
    fn read_step(
        &mut self,
        path: &Path,
        offset: u64,
        step_records: usize,
        outcome: &mut ReplayOutcome,
    ) -> Option<(usize, bool)> {
        use std::io::Seek;
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(std::io::SeekFrom::Start(offset)).ok()?;
        let mut reader = BufReader::new(file);
        self.batch.clear();
        let mut buf = [0u8; SEAL_SPILL_RECORD_SIZE];
        let mut records_read = 0usize;
        // One coded line per step, not per record, however many are damaged.
        let mut checksum_refused = 0usize;
        while records_read < step_records {
            match reader.read_exact(&mut buf) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    note_checksum_refused(path, checksum_refused);
                    return Some((records_read, true));
                }
                Err(err) => {
                    warn!(
                        ?path,
                        ?err,
                        "seal replay: read error in a staged spill file"
                    );
                    self.batch.clear();
                    return None;
                }
            }
            records_read += 1;
            // The same version and checksum gate as the boot drain.
            match decode_spill_record(&buf) {
                SpillRecordRead::Seal(seal) => match seal.try_into_buffered_seal() {
                    Some(seal) => self.batch.push(seal),
                    None => outcome.records_skipped += 1,
                },
                SpillRecordRead::OtherVersion => outcome.records_skipped += 1,
                SpillRecordRead::ChecksumMismatch => {
                    outcome.records_skipped += 1;
                    checksum_refused += 1;
                }
            }
        }
        note_checksum_refused(path, checksum_refused);
        // Exactly one step's worth read: the file may end right here, which
        // the next step discovers with a zero-record read.
        Some((records_read, false))
    }
}

/// Moves every live `.bin` seal spill file in `spill_dir` into `replaying/`,
/// with appends paused, and returns how many moved.
///
/// The directory is created and listed WITHOUT the append lock; only the
/// renames run under it, so an appender waits for a handful of `rename(2)`
/// calls at most. A file created after the listing is staged by the next scan.
fn stage_live_spill_files(spill: &crate::seal_spill::SealSpillWriter, spill_dir: &Path) -> usize {
    let live = live_spill_files(spill_dir);
    if live.is_empty() {
        return 0;
    }
    let replaying = spill_dir.join(SEAL_REPLAYING_SUBDIR);
    if let Err(err) = std::fs::create_dir_all(&replaying) {
        warn!(
            ?replaying,
            ?err,
            "seal replay: cannot create the staging dir"
        );
        return 0;
    }
    spill.with_appends_paused(|| {
        let mut moved = 0usize;
        let mut every_rename_ok = true;
        for path in &live {
            let Some(name) = path.file_name() else {
                every_rename_ok = false;
                continue;
            };
            let target = free_path(&replaying, name);
            match std::fs::rename(path, &target) {
                Ok(()) => moved += 1,
                Err(err) => {
                    every_rename_ok = false;
                    warn!(?path, ?err, "seal replay: cannot stage a spill file");
                }
            }
        }
        // Z6: the spill epoch closes only if NO live file is left behind. A
        // file created after the unlocked listing above, or one whose rename
        // failed, holds appends of the closing epoch; one more listing, under
        // the lock, says whether any is left. O(1) EXEMPT: one directory
        // listing per staging, at most once per scan interval.
        let moved_every_live_file = every_rename_ok && live_spill_files(spill_dir).is_empty();
        (moved, moved_every_live_file)
    })
}

/// Every live `.bin` seal spill file directly in `spill_dir`.
fn live_spill_files(spill_dir: &Path) -> Vec<PathBuf> {
    // O(1) EXEMPT: cold directory listing, at most once per scan interval
    std::fs::read_dir(spill_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_seal_file(p) && staged_kind(p) == Some(StagedKind::Spill))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// B2 (2026-09-22): the drain stages BOTH names - the current one it
    /// replays, and the legacy one whose stale records the version gate
    /// refuses and archives - and nothing else.
    #[test]
    fn the_drain_globs_the_current_and_the_legacy_prefix_only() {
        let dir = std::env::temp_dir().join(format!(
            "tv-seal-prefix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, want) in [
            ("seals_v4-2026-09-22.bin", true),
            ("seals_v4-2026-09-22.ndjson", true),
            ("seals-2026-09-18.bin", true),
            ("seals-2026-09-18.ndjson", true),
            ("seals_v4-2026-09-22.bin.1", true),
            ("seals_v5-2026-09-22.bin", false),
            ("notes-2026-09-22.bin", false),
            ("seals_v4-2026-09-22.txt", false),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            assert_eq!(is_seal_file(&p), want, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three unreadable paths must be DISTINGUISHABLE from an empty file.
    ///
    /// This is the whole defect. Until 2026-08-21 an unopenable spill file, an
    /// unopenable DLQ file, and a file whose name could not be classified all
    /// produced an empty record vec — byte-identical to "read fine, contained
    /// nothing". The caller then archived the file as fully recovered, so
    /// every seal inside it was lost permanently AND could never be retried,
    /// while boot printed its success line.
    ///
    /// `None` vs `Some(empty)` is the entire fix, so it is what the test
    /// asserts.
    #[test]
    fn an_unreadable_staged_file_is_none_not_an_empty_recovery() {
        let dir = std::env::temp_dir().join(format!("tv-seal-unreadable-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // A path that does not exist cannot be opened.
        let missing = dir.join("seal-spill-2026-08-21.bin");
        let _ = std::fs::remove_file(&missing);
        assert!(
            read_staged_spill(&missing).is_none(),
            "an unopenable spill file must report NOT-READ, never an empty recovery — \
             an empty recovery gets the file archived and its seals lost"
        );

        let missing_dlq = dir.join("seal-dlq-2026-08-21.ndjson");
        let _ = std::fs::remove_file(&missing_dlq);
        assert!(
            read_staged_dlq(&missing_dlq).is_none(),
            "an unopenable dlq file must report NOT-READ"
        );

        // Non-vacuity: a file that EXISTS and is genuinely empty must read as
        // a successful recovery of zero records, not as unreadable. Without
        // this the fix could have been "always return None", which would stage
        // every file forever.
        let empty = dir.join("seal-spill-2026-08-20.bin");
        std::fs::write(&empty, b"").expect("write empty spill");
        let read = read_staged_spill(&empty);
        assert!(
            read.is_some(),
            "an EMPTY but readable file is a successful recovery of nothing, and must \
             still be archivable — otherwise it stages forever"
        );
        assert_eq!(read.map(|(r, u)| (r.len(), u)), Some((0, 0)));

        let empty_dlq = dir.join("seal-dlq-2026-08-20.ndjson");
        std::fs::write(&empty_dlq, b"").expect("write empty dlq");
        assert_eq!(
            read_staged_dlq(&empty_dlq).map(|(r, u)| (r.len(), u)),
            Some((0, 0))
        );

        let _ = std::fs::remove_file(&empty);
        let _ = std::fs::remove_file(&empty_dlq);
        let _ = std::fs::remove_dir(&dir);
    }

    /// A filename the classifier does not recognise must not be silently
    /// archived either — that arm did not even log before this change.
    #[test]
    fn an_unclassifiable_staged_filename_is_refused_by_the_classifier() {
        assert!(
            staged_kind(Path::new("/tmp/replaying/not-a-seal-file.txt")).is_none(),
            "the classifier must refuse an unrecognised name, so the caller can \
             refuse to archive it"
        );
    }

    /// The recovery loop must refuse to archive on the not-read path, and must
    /// say so with a coded error rather than a bare warn.
    #[test]
    fn the_recovery_loop_refuses_to_archive_what_it_could_not_read() {
        let src = include_str!("seal_writer_task.rs");
        // Split on the module header, NOT on `#[cfg(test)]`: that attribute
        // also appears inside a doc comment earlier in this file, which
        // truncated the scan to the first 271 lines and made this test fail
        // against code it never reached.
        let prod = src.split("\nmod tests {").next().unwrap_or(src);
        assert!(
            prod.contains("outcome.files_left_pending += 1;\n            continue;"),
            "the not-read arm must count the file as still pending and skip it, \
             never fall through to archive_staged"
        );
        assert!(
            prod.contains("could not be read or classified"),
            "the not-read arm must name what happened"
        );
    }

    /// The BOOT DRAIN — not `SealSpillWriter::read_all`, which has no
    /// production caller — must refuse a spill record from any other format
    /// version, OLDER or NEWER.
    ///
    /// Until 2026-09-22 this reader refused only byte-7 == 0, so a version-3
    /// record (24-frame ordinal space) re-ingested under the nine-frame
    /// numbering: ordinal 5 was `S1` and is now `M30`.
    #[test]
    fn boot_drain_refuses_a_spill_record_from_another_format_version() {
        let dir = std::env::temp_dir().join(format!("tv-seal-version-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("seal-spill-2026-09-22.bin");

        let current =
            SerializedSeal::from(&mk_seal(13, 0, TfIndex::S1, 1_716_000_900, 102.5)).to_bytes();
        assert_eq!(current[7], SEAL_SPILL_FORMAT_VERSION);
        // 2026-10-01: the oldest readable version is 4, so "older" is the
        // version below it — the last one that renumbered the ordinals.
        let mut older = current;
        older[7] = crate::seal_spill::SEAL_SPILL_OLDEST_READABLE_VERSION - 1;
        let mut newer = current;
        newer[7] = SEAL_SPILL_FORMAT_VERSION + 1;
        let mut legacy_zero = current;
        legacy_zero[7] = 0;

        let mut bytes = Vec::new();
        for record in [current, older, newer, legacy_zero] {
            bytes.extend_from_slice(&record);
        }
        std::fs::write(&path, &bytes).expect("write spill");

        let (records, undecodable) = read_staged_spill(&path).expect("readable");
        assert_eq!(
            records.len(),
            1,
            "only the current-version record may be re-ingested"
        );
        assert_eq!(
            undecodable, 3,
            "the older, the newer (a rollback) and the version-0 record are all \
             refused and counted — never decoded under the wrong ordinal space"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// The DLQ had no format version at all until 2026-09-22, so every line it
    /// ever wrote decoded under whatever ordinal space the reading binary had.
    /// A line with no `format_version` (every pre-2026-09-22 line) and a line
    /// stamped with another version must both be refused; a line this build
    /// writes must survive.
    #[test]
    fn boot_drain_refuses_an_unversioned_dlq_line() {
        let dir = std::env::temp_dir().join(format!("tv-seal-dlq-version-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("seal-dlq-2026-09-22.ndjson");

        let written = SealDlqRecord::from(&SerializedSeal::from(&mk_seal(
            13,
            0,
            TfIndex::M1,
            1_716_000_900,
            102.5,
        )));
        assert_eq!(
            written.format_version, SEAL_SPILL_FORMAT_VERSION,
            "every line this build writes is stamped"
        );
        let current_line = serde_json::to_string(&written).expect("serialise");

        let mut unversioned: serde_json::Value =
            serde_json::from_str(&current_line).expect("round-trip");
        if let Some(map) = unversioned.as_object_mut() {
            map.remove("format_version");
        }
        let mut rollback = written.clone();
        rollback.format_version = SEAL_SPILL_FORMAT_VERSION + 1;

        let body = format!(
            "{current_line}\n{}\n{}\n",
            serde_json::to_string(&unversioned).expect("serialise"),
            serde_json::to_string(&rollback).expect("serialise"),
        );
        std::fs::write(&path, body).expect("write dlq");

        let (records, undecodable) = read_staged_dlq(&path).expect("readable");
        assert_eq!(records.len(), 1, "only the stamped current line survives");
        assert_eq!(
            undecodable, 2,
            "an unversioned line and a newer-version line are both refused and counted"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Audit PR41c: the boot drain reads a version-4 record (the build before
    /// this one wrote it) and refuses a damaged version-5 record without
    /// losing the records after it.
    #[test]
    fn boot_drain_reads_a_v4_record_and_refuses_a_damaged_one() {
        let dir = std::env::temp_dir().join(format!("tv-seal-pr41c-boot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("seals_v4-2026-10-01.bin");

        let good = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 102.5));
        let after = SerializedSeal::from(&mk_seal(14, 0, TfIndex::M1, 1_716_000_900, 103.5));
        let mut v4 = good.to_bytes();
        v4[0..4].copy_from_slice(&13_u32.to_le_bytes());
        v4[7] = 4;
        let mut damaged = good.to_bytes();
        damaged[64] ^= 0x40;

        let mut bytes = Vec::new();
        for record in [good.to_bytes(), v4, damaged, after.to_bytes()] {
            bytes.extend_from_slice(&record);
        }
        std::fs::write(&path, &bytes).expect("write spill");

        let (records, undecodable) = read_staged_spill(&path).expect("readable");
        assert_eq!(records, vec![good, good, after]);
        assert_eq!(undecodable, 1, "the damaged record is refused and counted");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Audit PR41c: a dead-letter line stamped version 4 is still read.
    #[test]
    fn boot_drain_reads_a_v4_dlq_line() {
        let dir = std::env::temp_dir().join(format!("tv-seal-pr41c-dlq-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("seals_v4-2026-10-01.ndjson");
        let mut line = SealDlqRecord::from(&SerializedSeal::from(&mk_seal(
            13,
            0,
            TfIndex::M1,
            1_716_000_900,
            102.5,
        )));
        line.format_version = crate::seal_spill::SEAL_SPILL_OLDEST_READABLE_VERSION;
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&line).expect("serialise")),
        )
        .expect("write dlq");
        let (records, undecodable) = read_staged_dlq(&path).expect("readable");
        assert_eq!((records.len(), undecodable), (1, 0));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
    use std::path::PathBuf;
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
        let mut spill = std::env::temp_dir();
        let mut dlq = std::env::temp_dir();
        spill.push(format!(
            "tickvault-seal-writer-task-spill-{}-{}",
            name,
            std::process::id()
        ));
        dlq.push(format!(
            "tickvault-seal-writer-task-dlq-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&spill);
        let _ = std::fs::remove_dir_all(&dlq);
        std::fs::create_dir_all(&spill).expect("spill dir");
        std::fs::create_dir_all(&dlq).expect("dlq dir");
        (spill, dlq)
    }

    fn cleanup(spill: &PathBuf, dlq: &PathBuf) {
        let _ = std::fs::remove_dir_all(spill);
        let _ = std::fs::remove_dir_all(dlq);
    }

    fn jan1_noon_utc() -> i64 {
        chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    fn mk_seal(sid: u64, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.open = 100.0;
        state.high = 105.0;
        state.low = 99.0;
        state.close = close;
        state.volume = 1234;
        state.bucket_start_cumulative = 1000;
        state.oi = 50_000;
        state.tick_count = 5;
        state.close_pct_from_prev_day = 1.5;
        state.net_volume_signed = -4_242;
        state.net_volume_classified = true;
        state.total_buy_qty = 89_600;
        state.total_sell_qty = 4_800;
        BufferedSeal::new(sid, seg, tf, state, Feed::Dhan)
    }

    #[test]
    fn test_drain_once_returns_idle_when_ring_is_empty() {
        let (spill, dlq) = temp_pair("idle");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, jan1_noon_utc());
        assert!(outcome.is_idle());
        assert_eq!(outcome.ring_seals_popped, 0);
        assert!(!outcome.flushed_ok);
        assert!(!outcome.has_rescues());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_with_max_drain_zero_is_idle() {
        let (spill, dlq) = temp_pair("max-zero");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        pipeline.submit(
            mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0),
            jan1_noon_utc(),
        );
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 0, jan1_noon_utc());
        assert!(outcome.is_idle());
        assert_eq!(pipeline.ring_len(), 1, "ring untouched on max_drain=0");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_caps_at_max_drain() {
        // Ring has 5 seals, max_drain is 3 → only 3 popped.
        let (spill, dlq) = temp_pair("cap");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        for i in 0..5 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_023_700 + i as u32,
                    100.0 + i as f64,
                ),
                jan1_noon_utc(),
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 3, jan1_noon_utc());
        assert_eq!(outcome.ring_seals_popped, 3);
        assert_eq!(pipeline.ring_len(), 2, "2 seals must remain in ring");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_disconnected_writer_rescues_all_to_spill() {
        // ShadowCandleWriter::for_test() always errs on flush. With
        // a healthy spill dir, every popped seal must rescue to
        // spill (tier 2) successfully.
        let (spill, dlq) = temp_pair("rescue-spill");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..5 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_023_700 + i as u32,
                    100.0 + i as f64,
                ),
                now,
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 5);
        assert!(!outcome.flushed_ok, "disconnected writer can't flush");
        assert_eq!(outcome.rescued_to_spill, 5);
        assert_eq!(outcome.rescued_to_dlq, 0);
        assert_eq!(outcome.rescued_dropped, 0);
        assert_eq!(pipeline.ring_len(), 0, "ring fully drained");

        // Verify spill file actually contains the 5 evicted seals.
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 5);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_discards_writer_buffer_after_flush_failure_rescue() {
        // Poison-buffer regression (2026-07-06 hostile-review fix): after a
        // failed flush drain_once rescues the popped seals to spill/DLQ and
        // MUST discard the writer's retained ILP buffer. Otherwise (a) a
        // server-rejected row replays on every later flush — one poisoned row
        // permanently kills candle persistence — and (b) the buffer grows
        // without bound during a sustained outage, wedging at the questdb-rs
        // 100 MiB max_buf_size even after QuestDB recovers.
        let (spill, dlq) = temp_pair("discard-after-rescue");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0), now);
        pipeline.submit(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0), now);
        let mut writer = ShadowCandleWriter::for_test();

        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 2);
        assert!(!outcome.flushed_ok);
        assert_eq!(outcome.rescued_to_spill, 2, "both durably rescued");
        assert_eq!(
            writer.pending_count(),
            0,
            "writer pending MUST be discarded after rescue (poison-proof)"
        );
        assert_eq!(
            writer.buffer_byte_count(),
            0,
            "writer ILP buffer MUST be empty after rescue — no cross-cycle replay/growth"
        );

        // A SECOND failed cycle must not accumulate the first cycle's bytes:
        // the buffer is bounded to one drain batch forever.
        pipeline.submit(mk_seal(51, 0, TfIndex::M1, 1_716_024_900, 300.0), now);
        let outcome2 = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome2.ring_seals_popped, 1);
        assert_eq!(outcome2.rescued_to_spill, 1);
        assert_eq!(writer.buffer_byte_count(), 0, "still clean after cycle 2");
        assert_eq!(writer.pending_count(), 0);

        // All 3 seals are durably in spill — nothing was lost by discarding.
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 3);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_disconnected_writer_rescues_to_dlq_when_spill_blocked() {
        // Block tier-2 spill by pointing it at a regular file (so
        // create_dir_all errs); tier-3 DLQ has a real dir → rescue
        // lands in DLQ.
        let mut spill_blocker = std::env::temp_dir();
        spill_blocker.push(format!(
            "tickvault-seal-writer-task-spill-blocker-{}-{}",
            "rescue-dlq",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_blocker);
        let _ = std::fs::remove_dir_all(&spill_blocker);
        std::fs::write(&spill_blocker, b"not a dir").expect("create blocker");

        let (_spill_unused, dlq) = temp_pair("rescue-dlq");
        let mut pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            8,
            spill_blocker.clone(),
            dlq.clone(),
        );
        let now = jan1_noon_utc();
        for i in 0..3 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_023_700 + i as u32,
                    100.0 + i as f64,
                ),
                now,
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 3);
        assert!(!outcome.flushed_ok);
        assert_eq!(outcome.rescued_to_spill, 0);
        assert_eq!(outcome.rescued_to_dlq, 3);
        assert_eq!(outcome.rescued_dropped, 0);

        // Verify DLQ file actually contains 3 records.
        let dlq_writer = crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(dlq.clone());
        let drained = dlq_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 3);

        let _ = std::fs::remove_file(&spill_blocker);
        cleanup(&spill_blocker, &dlq);
    }

    #[test]
    fn test_drain_once_disconnected_writer_drops_when_spill_and_dlq_blocked() {
        // Block BOTH tier-2 + tier-3 → rescued_dropped counter
        // increments for every popped seal.
        let mut spill_blocker = std::env::temp_dir();
        spill_blocker.push(format!(
            "tickvault-seal-writer-task-spill-blocker2-{}-{}",
            "rescue-dropped",
            std::process::id()
        ));
        let mut dlq_blocker = std::env::temp_dir();
        dlq_blocker.push(format!(
            "tickvault-seal-writer-task-dlq-blocker-{}-{}",
            "rescue-dropped",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&spill_blocker);
        let _ = std::fs::remove_dir_all(&spill_blocker);
        let _ = std::fs::remove_file(&dlq_blocker);
        let _ = std::fs::remove_dir_all(&dlq_blocker);
        std::fs::write(&spill_blocker, b"file").expect("blocker");
        std::fs::write(&dlq_blocker, b"file").expect("blocker");

        let mut pipeline = SealAbsorptionPipeline::with_capacity_and_dirs_for_test(
            8,
            spill_blocker.clone(),
            dlq_blocker.clone(),
        );
        let now = jan1_noon_utc();
        for i in 0..2 {
            pipeline.submit(
                mk_seal(
                    13 + i,
                    0,
                    TfIndex::M1,
                    1_716_023_700 + i as u32,
                    100.0 + i as f64,
                ),
                now,
            );
        }
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 2);
        assert!(!outcome.flushed_ok);
        assert_eq!(outcome.rescued_to_spill, 0);
        assert_eq!(outcome.rescued_to_dlq, 0);
        assert_eq!(
            outcome.rescued_dropped, 2,
            "both seals MUST count as dropped when spill+DLQ blocked"
        );

        let _ = std::fs::remove_file(&spill_blocker);
        let _ = std::fs::remove_file(&dlq_blocker);
    }

    #[test]
    fn test_drain_outcome_default_is_zeroed_idle() {
        let o = DrainOutcome::default();
        assert!(o.is_idle());
        assert!(!o.has_rescues());
        assert_eq!(o.ring_seals_popped, 0);
        assert!(!o.flushed_ok);
        assert_eq!(o.rescued_to_spill, 0);
        assert_eq!(o.rescued_to_dlq, 0);
        assert_eq!(o.rescued_dropped, 0);
    }

    #[test]
    fn test_drain_outcome_is_idle_returns_false_when_seals_popped() {
        let o = DrainOutcome {
            ring_seals_popped: 1,
            ..DrainOutcome::default()
        };
        assert!(!o.is_idle());
    }

    #[test]
    fn test_drain_outcome_has_rescues_for_each_kind() {
        for o in [
            DrainOutcome {
                rescued_to_spill: 1,
                ..DrainOutcome::default()
            },
            DrainOutcome {
                rescued_to_dlq: 1,
                ..DrainOutcome::default()
            },
            DrainOutcome {
                rescued_dropped: 1,
                ..DrainOutcome::default()
            },
        ] {
            assert!(o.has_rescues(), "{o:?} must report has_rescues");
        }
    }

    #[test]
    fn test_drain_once_does_not_re_buffer_rescued_seals_in_ring() {
        // FIFO invariant: rescued seals MUST go straight to spill,
        // NOT back into the ring. Verify ring_len returns to 0 even
        // though flush failed.
        let (spill, dlq) = temp_pair("no-rebuffer");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0), now);
        pipeline.submit(mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0), now);
        let mut writer = ShadowCandleWriter::for_test();
        drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(
            pipeline.ring_len(),
            0,
            "rescued seals MUST go to spill, NOT re-buffer into ring"
        );
        cleanup(&spill, &dlq);
    }

    /// Audit PR31a: a flush refused because the candle tables are not yet
    /// keyed is not a database failure, but its seals must still reach the
    /// spill — the quieter log line must never mean a quieter rescue.
    #[test]
    fn drain_once_rescues_every_seal_when_the_candle_tables_are_not_keyed() {
        static LOCAL_KEYED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        let (spill, dlq) = temp_pair("unkeyed");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(64, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..3 {
            pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700 + i, 100.0), now);
        }
        let mut writer = ShadowCandleWriter::for_test().with_keyed_for_test(&LOCAL_KEYED);
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert!(!outcome.flushed_ok);
        assert_eq!(outcome.ring_seals_popped, 3);
        assert_eq!(
            outcome.rescued_to_spill + outcome.rescued_to_dlq,
            3,
            "every refused seal must be rescued"
        );
        assert_eq!(
            writer.pending_count(),
            0,
            "the refused buffer is discarded after rescue"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn drain_once_leaves_the_remainder_in_the_ring_when_the_cap_binds() {
        // The prod defect this pins, 2026-08-20: the drain sat at EXACTLY
        // `cycles × max_drain` for hours while 61% of sealed candles went to
        // disk instead of the database, and NOTHING said the cap was the
        // reason. `flush_failures` was 0, `dropped` was 0, seals were being
        // submitted and rows written — every existing counter looked healthy.
        //
        // The cap binding and the cap fitting must be distinguishable, because
        // their fixes are opposite: one raises a constant, the other means the
        // database genuinely cannot keep up.
        let (spill, dlq) = temp_pair("cap-binds");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(64, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..10 {
            pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700 + i, 100.0), now);
        }
        let mut writer = ShadowCandleWriter::for_test();

        // Cap of 4 against 10 waiting: takes exactly 4, and 6 stay queued.
        let outcome = drain_once(&mut pipeline, &mut writer, 4, now);
        assert_eq!(
            outcome.ring_seals_popped, 4,
            "the cap must bind at exactly its value"
        );
        assert_eq!(
            pipeline.ring_len(),
            6,
            "the remainder must still be in the ring, not silently gone"
        );

        cleanup(&spill, &dlq);
    }

    #[test]
    fn drain_once_takes_everything_when_the_cap_does_not_bind() {
        // The other half of the same distinction. A generous cap against a
        // short ring must drain it completely and leave nothing behind —
        // otherwise "the cap binds" could never be told apart from "there was
        // nothing more to take", which is precisely the confusion that let the
        // live ceiling hide.
        let (spill, dlq) = temp_pair("cap-free");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(64, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        for i in 0..3 {
            pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700 + i, 100.0), now);
        }
        let mut writer = ShadowCandleWriter::for_test();

        let outcome = drain_once(&mut pipeline, &mut writer, 4_096, now);
        assert_eq!(outcome.ring_seals_popped, 3, "all three taken");
        assert_eq!(pipeline.ring_len(), 0, "ring fully drained");

        cleanup(&spill, &dlq);
    }

    #[test]
    fn drain_once_reserves_for_what_waits_not_for_the_cap() {
        // With the cap at 16,384 a mostly-empty ring would reserve ~2.4 MB
        // ten times a second for a handful of seals. The reservation follows
        // `ring_len` instead — asserted through behaviour, since capacity
        // itself is not observable from here: a huge cap against one seal must
        // still pop exactly one and drain the ring, with no panic and no
        // re-allocation path taken.
        let (spill, dlq) = temp_pair("cap-reserve");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0), now);
        let mut writer = ShadowCandleWriter::for_test();

        let outcome = drain_once(&mut pipeline, &mut writer, 16_384, now);
        assert_eq!(outcome.ring_seals_popped, 1);
        assert_eq!(pipeline.ring_len(), 0);

        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_drain_once_preserves_seal_identity_through_rescue() {
        // I-P1-11 + payload integrity: same security_id with two
        // different segments rescues both records to spill, distinct.
        let (spill, dlq) = temp_pair("identity");
        let mut pipeline =
            SealAbsorptionPipeline::with_capacity_and_dirs_for_test(8, spill.clone(), dlq.clone());
        let now = jan1_noon_utc();
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0), now);
        pipeline.submit(mk_seal(13, 1, TfIndex::M1, 1_716_024_300, 200.0), now);
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(outcome.ring_seals_popped, 2);
        assert_eq!(outcome.rescued_to_spill, 2);

        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        // First popped = oldest = seg 0 (added first).
        assert_eq!(drained[0].security_id, 13);
        assert_eq!(drained[0].exchange_segment_code, 0);
        assert_eq!(drained[0].close, 100.0);
        assert_eq!(drained[1].security_id, 13);
        assert_eq!(drained[1].exchange_segment_code, 1);
        assert_eq!(drained[1].close, 200.0);
        cleanup(&spill, &dlq);
    }

    // -----------------------------------------------------------------------
    // Mid-session spill replay (audit PR15)
    // -----------------------------------------------------------------------

    /// A sink that records what it committed and can be told to fail flushes.
    #[derive(Default)]
    struct ReplaySink {
        pending: Vec<u32>,
        committed: Vec<u32>,
        fail_flushes: usize,
        flushes: usize,
        /// A bucket the database refuses: any flush carrying it fails.
        poison: Option<u32>,
        /// Report the poison bucket's failure as a transport error (audit
        /// PR41b), as a flapping connection would, instead of a refusal.
        poison_as_socket: bool,
    }

    impl SealSink for ReplaySink {
        fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
            self.pending.push(seal.state.bucket_start_ist_secs);
            Ok(())
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            self.flushes += 1;
            if self.fail_flushes > 0 {
                self.fail_flushes -= 1;
                anyhow::bail!("injected flush failure");
            }
            if self.poison.is_some_and(|p| self.pending.contains(&p)) {
                if self.poison_as_socket {
                    return Err(anyhow::Error::new(questdb::Error::new(
                        questdb::ErrorCode::SocketError,
                        "injected: connection reset",
                    ))
                    .context("shadow flush: ILP send failed after reconnect retries"));
                }
                anyhow::bail!("injected poison row");
            }
            self.committed.append(&mut self.pending);
            Ok(())
        }
        fn discard_pending(&mut self) {
            self.pending.clear();
        }
    }

    fn healthy_drain() -> DrainOutcome {
        DrainOutcome {
            ring_seals_popped: 1,
            flushed_ok: true,
            ..DrainOutcome::default()
        }
    }

    fn failed_drain() -> DrainOutcome {
        DrainOutcome {
            ring_seals_popped: 1,
            flushed_ok: false,
            ..DrainOutcome::default()
        }
    }

    fn spill_n(writer: &crate::seal_spill::SealSpillWriter, n: u32, now: i64) {
        for i in 0..n {
            let seal = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, i, 101.5));
            writer.append_seal(&seal, now).expect("spill append");
        }
    }

    fn count_bin(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| is_seal_file(&e.path()))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn replay_waits_for_sixty_seconds_of_clean_flushes() {
        let (spill, dlq) = temp_pair("replay-health-gate");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 10, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();

        // Nothing observed yet: no clock, no replay.
        assert!(
            replay
                .step(&mut sink, &writer, &spill, true, t0 + 600)
                .is_idle()
        );
        replay.observe(&healthy_drain(), t0);
        assert!(
            replay
                .step(
                    &mut sink,
                    &writer,
                    &spill,
                    true,
                    t0 + SEAL_REPLAY_HEALTHY_SECS - 1
                )
                .is_idle(),
            "59 healthy seconds are not 60"
        );
        assert_eq!(
            count_bin(&spill),
            1,
            "the live file is untouched before the gate opens"
        );

        let out = replay.step(
            &mut sink,
            &writer,
            &spill,
            true,
            t0 + SEAL_REPLAY_HEALTHY_SECS,
        );
        assert_eq!(out.files_staged, 1);
        assert_eq!(out.seals_reingested, 10);
        assert_eq!(out.files_archived, 1);
        assert_eq!(sink.committed, (0..10).collect::<Vec<_>>());
        assert_eq!(count_bin(&spill.join(SEAL_ARCHIVE_SUBDIR)), 1);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_failed_live_flush_resets_the_health_gate() {
        let t0 = jan1_noon_utc();
        let mut replay = MidSessionReplay::default();
        replay.observe(&healthy_drain(), t0);
        assert!(replay.is_healthy(t0 + 60));
        replay.observe(&failed_drain(), t0 + 61);
        assert!(
            !replay.is_healthy(t0 + 200),
            "a failure must restart the clock"
        );
        // An idle cycle says nothing about the database.
        replay.observe(&DrainOutcome::default(), t0 + 62);
        assert!(!replay.is_healthy(t0 + 300));
        replay.observe(&healthy_drain(), t0 + 400);
        assert!(!replay.is_healthy(t0 + 459));
        assert!(replay.is_healthy(t0 + 460));
    }

    #[test]
    fn replay_never_runs_behind_live_work() {
        let (spill, dlq) = temp_pair("replay-live-first");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 5, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);
        assert!(
            replay
                .step(&mut sink, &writer, &spill, false, t0 + 120)
                .is_idle(),
            "a ring with live seals waiting must not be delayed by a replay"
        );
        assert_eq!(count_bin(&spill), 1);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn replay_is_rate_capped_per_step() {
        let (spill, dlq) = temp_pair("replay-rate-cap");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        let total = u32::try_from(SEAL_REPLAY_SEALS_PER_CYCLE * 2 + 452).expect("fits");
        spill_n(&writer, total, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);
        let now = t0 + 60;

        let first = replay.step(&mut sink, &writer, &spill, true, now);
        assert_eq!(first.seals_reingested, SEAL_REPLAY_SEALS_PER_CYCLE);
        assert_eq!(first.files_archived, 0);
        let second = replay.step(&mut sink, &writer, &spill, true, now);
        assert_eq!(second.seals_reingested, SEAL_REPLAY_SEALS_PER_CYCLE);
        let third = replay.step(&mut sink, &writer, &spill, true, now);
        assert_eq!(third.seals_reingested, 452);
        assert_eq!(third.files_archived, 1);
        assert_eq!(sink.flushes, 3, "one flush per step");
        assert_eq!(sink.committed, (0..total).collect::<Vec<_>>());
        assert!(replay.current_file().is_none());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_failed_replay_flush_keeps_the_position_and_waits_for_health() {
        let (spill, dlq) = temp_pair("replay-flush-fail");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        let total = u32::try_from(SEAL_REPLAY_SEALS_PER_CYCLE + 10).expect("fits");
        spill_n(&writer, total, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);

        let ok = replay.step(&mut sink, &writer, &spill, true, t0 + 60);
        assert_eq!(ok.seals_reingested, SEAL_REPLAY_SEALS_PER_CYCLE);
        sink.fail_flushes = 1;
        let failed = replay.step(&mut sink, &writer, &spill, true, t0 + 60);
        assert!(failed.flush_failed);
        assert_eq!(failed.seals_reingested, 0);
        assert!(
            sink.pending.is_empty(),
            "the failed batch must be discarded, not retained"
        );
        assert!(!replay.is_healthy(t0 + 61));
        assert!(
            replay
                .step(&mut sink, &writer, &spill, true, t0 + 100)
                .is_idle(),
            "no replay until the database is healthy again"
        );
        let staged = replay
            .current_file()
            .expect("the file keeps its place")
            .to_path_buf();
        assert!(staged.exists(), "a failed flush must leave the file staged");
        assert_eq!(
            count_bin(&spill),
            0,
            "a failed replay must never re-spill what it read"
        );

        replay.observe(&healthy_drain(), t0 + 200);
        let resumed = replay.step(&mut sink, &writer, &spill, true, t0 + 260);
        assert_eq!(
            resumed.seals_reingested, 10,
            "resumes at the first uncommitted record"
        );
        assert_eq!(resumed.files_archived, 1);
        assert_eq!(
            sink.committed,
            (0..total).collect::<Vec<_>>(),
            "each seal committed once"
        );
        cleanup(&spill, &dlq);
    }

    /// Drive the replay until it archives the file or `max_steps` run out,
    /// reopening the health gate after every failed step exactly as a healthy
    /// live writer would. Returns the summed outcome.
    fn run_replay_to_end(
        replay: &mut MidSessionReplay,
        sink: &mut ReplaySink,
        writer: &crate::seal_spill::SealSpillWriter,
        spill: &Path,
        mut now: i64,
        max_steps: usize,
    ) -> ReplayOutcome {
        let mut total = ReplayOutcome::default();
        replay.observe(&healthy_drain(), now);
        for _ in 0..max_steps {
            now += SEAL_REPLAY_HEALTHY_SECS;
            let step = replay.step(sink, writer, spill, true, now);
            total.seals_reingested += step.seals_reingested;
            total.records_skipped += step.records_skipped;
            total.files_archived += step.files_archived;
            if step.flush_failed {
                replay.observe(&healthy_drain(), now);
            }
            if total.files_archived > 0 {
                break;
            }
        }
        total
    }

    #[test]
    fn one_poison_record_is_isolated_and_skipped_and_every_other_record_lands() {
        let (spill, dlq) = temp_pair("replay-poison");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        let total = u32::try_from(SEAL_REPLAY_SEALS_PER_CYCLE + 300).expect("fits");
        spill_n(&writer, total, t0);
        let poison = 777;
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink {
            poison: Some(poison),
            ..ReplaySink::default()
        };

        let sum = run_replay_to_end(&mut replay, &mut sink, &writer, &spill, t0, 200);

        assert_eq!(
            sum.files_archived, 1,
            "the file must not stall on one bad row"
        );
        assert_eq!(
            sum.records_skipped, 1,
            "exactly the poison record is skipped"
        );
        let expected: Vec<u32> = (0..total).filter(|b| *b != poison).collect();
        assert_eq!(
            sink.committed, expected,
            "every other record lands, in order"
        );
        cleanup(&spill, &dlq);
    }

    // -----------------------------------------------------------------
    // Audit PR41b: flap vs bad record, stuck files, the QuestDB watcher.
    // -----------------------------------------------------------------

    /// Spill `n` seals with buckets `first..first + n`, into the day file of
    /// `now`.
    fn spill_range(writer: &crate::seal_spill::SealSpillWriter, first: u32, n: u32, now: i64) {
        for i in first..first + n {
            let seal = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, i, 101.5));
            writer.append_seal(&seal, now).expect("spill append");
        }
    }

    /// One healthy-gated step every `SEAL_REPLAY_HEALTHY_SECS`, reopening the
    /// gate with a clean live flush after a failed step. Stops when `done`.
    fn drive(
        replay: &mut MidSessionReplay,
        sink: &mut ReplaySink,
        writer: &crate::seal_spill::SealSpillWriter,
        spill: &Path,
        now: &mut i64,
        max_steps: usize,
        done: impl Fn(&ReplayOutcome, &ReplaySink) -> bool,
    ) -> ReplayOutcome {
        let mut total = ReplayOutcome::default();
        replay.observe(&healthy_drain(), *now);
        for _ in 0..max_steps {
            *now += SEAL_REPLAY_HEALTHY_SECS;
            let step = replay.step(sink, writer, spill, true, *now);
            total.seals_reingested += step.seals_reingested;
            total.records_skipped += step.records_skipped;
            total.files_archived += step.files_archived;
            total.files_parked += step.files_parked;
            if step.flush_failed {
                replay.observe(&healthy_drain(), *now);
            }
            if done(&total, sink) {
                break;
            }
        }
        total
    }

    #[test]
    fn classify_replay_flush_failure_tells_the_transport_from_a_refusal() {
        let socket = anyhow::Error::new(questdb::Error::new(
            questdb::ErrorCode::SocketError,
            "Broken pipe",
        ))
        .context("shadow flush: ILP send failed after reconnect retries");
        assert_eq!(
            classify_replay_flush_failure(&socket),
            ReplayFlushFailure::Environment,
            "a transport failure under a context layer is still the transport"
        );
        let refused = anyhow::Error::new(questdb::Error::new(
            questdb::ErrorCode::ServerFlushError,
            "Could not flush buffer: bad value [line: 1]",
        ))
        .context("shadow flush: ILP send failed (non-connection)");
        assert_eq!(
            classify_replay_flush_failure(&refused),
            ReplayFlushFailure::Refused
        );
        let not_keyed = anyhow::Error::new(crate::shadow_candle_writer::CandleTablesNotKeyed);
        assert_eq!(
            classify_replay_flush_failure(&not_keyed),
            ReplayFlushFailure::Environment,
            "tables not yet keyed is the database's state, not the record's"
        );
        for code in [
            questdb::ErrorCode::CouldNotResolveAddr,
            questdb::ErrorCode::AuthError,
            questdb::ErrorCode::TlsError,
            questdb::ErrorCode::HttpNotSupported,
            questdb::ErrorCode::ConfigError,
        ] {
            let err = anyhow::Error::new(questdb::Error::new(code, "x"));
            assert_eq!(
                classify_replay_flush_failure(&err),
                ReplayFlushFailure::Environment,
                "{code:?}"
            );
        }
        assert_eq!(
            classify_replay_flush_failure(&anyhow::anyhow!("unknown")),
            ReplayFlushFailure::Refused,
            "an unknown cause can be the record"
        );
    }

    #[test]
    fn a_transport_failure_never_skips_a_record_and_a_stuck_file_is_parked_so_the_next_goes_ahead()
    {
        let (spill, dlq) = temp_pair("replay-stuck-park");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        // Two day files: the first holds a record whose every flush dies on
        // the connection, the second is clean.
        spill_range(&writer, 0, 40, t0);
        spill_range(&writer, 10_000, 30, t0 + 86_400);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink {
            poison: Some(17),
            poison_as_socket: true,
            ..ReplaySink::default()
        };
        let mut now = t0 + 2 * 86_400;
        let total = drive(
            &mut replay,
            &mut sink,
            &writer,
            &spill,
            &mut now,
            200,
            |_, sink| sink.committed.contains(&10_029),
        );

        assert_eq!(
            total.records_skipped, 0,
            "a transport failure is never held against the record"
        );
        assert_eq!(total.files_parked, 1, "the stuck file is parked once");
        let second: Vec<u32> = (10_000..10_030).collect();
        assert!(
            second.iter().all(|b| sink.committed.contains(b)),
            "the next file went ahead of the stuck one"
        );
        assert!(
            !sink.committed.contains(&17),
            "the record that never flushed is not reported as written"
        );
        let parked = replay.parked.first().expect("the stuck file is parked");
        assert_eq!(
            parked.cursor.offset,
            17 * SEAL_SPILL_RECORD_SIZE as u64,
            "the parked file keeps its place at the stuck record"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_parked_file_resumes_at_its_record_after_the_park_window() {
        let (spill, dlq) = temp_pair("replay-park-resume");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_range(&writer, 0, 20, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink {
            poison: Some(5),
            poison_as_socket: true,
            ..ReplaySink::default()
        };
        let mut now = t0 + 100;
        drive(
            &mut replay,
            &mut sink,
            &writer,
            &spill,
            &mut now,
            200,
            |total, _| total.files_parked > 0,
        );
        assert_eq!(replay.parked.len(), 1);
        assert_eq!(sink.committed, (0..5).collect::<Vec<u32>>());

        // The connection heals. Inside the park window the file waits.
        sink.poison = None;
        replay.observe(&healthy_drain(), now);
        now += SEAL_REPLAY_HEALTHY_SECS;
        let early = replay.step(&mut sink, &writer, &spill, true, now);
        assert_eq!(early.seals_reingested, 0, "parked until its retry time");

        now += SEAL_REPLAY_PARK_SECS;
        let total = drive(
            &mut replay,
            &mut sink,
            &writer,
            &spill,
            &mut now,
            50,
            |total, _| total.files_archived > 0,
        );
        assert_eq!(total.files_archived, 1);
        assert_eq!(
            sink.committed,
            (0..20).collect::<Vec<u32>>(),
            "resumed at the stuck record: nothing re-sent, nothing missed"
        );
        assert!(replay.parked.is_empty());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_replay_probe_current_reads_the_process_watermark() {
        let wm = crate::wal_applied_watermark::applied_watermark();
        let probe = ReplayProbe::current();
        // The watcher is not running under test, so the counter can only be
        // what the global reports; read both and compare.
        assert_eq!(probe.clean_probes, wm.clean_probe_count());
        assert_eq!(probe.suspect, wm.is_sink_suspect());
        assert!(
            !ReplayProbe::default().is_running(),
            "no probe seen reads as no watcher"
        );
    }

    #[test]
    fn a_clean_probe_reopens_the_gate_without_live_traffic() {
        let t0 = jan1_noon_utc();
        let mut replay = MidSessionReplay::default();
        replay.observe(&healthy_drain(), t0);
        replay.observe(&failed_drain(), t0 + 10);
        assert!(
            !replay.is_healthy(t0 + 3_600),
            "no live flush and no probe: the gate stays shut"
        );

        let probe = ReplayProbe {
            clean_probes: 1,
            suspect: false,
        };
        assert_eq!(replay.observe_probe(probe, t0 + 20), 0);
        assert!(
            !replay.is_healthy(t0 + 10 + SEAL_REPLAY_HEALTHY_SECS - 1),
            "a clean probe still waits out the quiet period after the failure"
        );
        assert!(
            replay.is_healthy(t0 + 10 + SEAL_REPLAY_HEALTHY_SECS),
            "a clean probe after the failure reopens the gate"
        );

        // A probe count that has not moved since a failure is not evidence.
        replay.observe(&failed_drain(), t0 + 200);
        assert!(!replay.is_healthy(t0 + 3_600));
    }

    #[test]
    fn a_suspect_table_closes_the_gate() {
        let t0 = jan1_noon_utc();
        let mut replay = MidSessionReplay::default();
        replay.observe(&healthy_drain(), t0);
        assert!(replay.is_healthy(t0 + SEAL_REPLAY_HEALTHY_SECS));
        replay.observe_probe(
            ReplayProbe {
                clean_probes: 3,
                suspect: true,
            },
            t0 + SEAL_REPLAY_HEALTHY_SECS,
        );
        replay.observe(&healthy_drain(), t0 + 100);
        assert!(
            !replay.is_healthy(t0 + 1_000),
            "clean live flushes do not outvote a suspended table"
        );
    }

    #[test]
    fn a_finished_file_waits_for_two_clean_probes_before_it_is_archived() {
        let (spill, dlq) = temp_pair("replay-archive-confirm");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_range(&writer, 0, 10, t0);
        let mut replay = MidSessionReplay::default();
        let mut probe = ReplayProbe {
            clean_probes: 1,
            suspect: false,
        };
        replay.observe_probe(probe, t0);
        let mut sink = ReplaySink::default();
        let mut now = t0 + 100;
        let read = drive(
            &mut replay,
            &mut sink,
            &writer,
            &spill,
            &mut now,
            10,
            |_, sink| sink.committed.len() == 10,
        );
        // One more step discovers the end of the file.
        now += SEAL_REPLAY_HEALTHY_SECS;
        let end = replay.step(&mut sink, &writer, &spill, true, now);
        assert_eq!(read.files_archived + end.files_archived, 0);
        assert_eq!(replay.awaiting_archive.len(), 1, "read, not yet confirmed");

        probe.clean_probes = 2;
        replay.observe_probe(probe, now);
        let one = replay.step(&mut sink, &writer, &spill, true, now + 1);
        assert_eq!(one.files_archived, 0, "one probe may have started earlier");

        probe.clean_probes = 3;
        replay.observe_probe(probe, now);
        let two = replay.step(&mut sink, &writer, &spill, true, now + 2);
        assert_eq!(two.files_archived, 1, "two clean probes confirm the file");
        assert_eq!(count_bin(&spill.join(SEAL_ARCHIVE_SUBDIR)), 1);
        assert_eq!(sink.committed.len(), 10, "nothing written twice");
        cleanup(&spill, &dlq);
    }

    #[test]
    fn suspicion_before_confirmation_reads_the_file_again_from_its_start() {
        let (spill, dlq) = temp_pair("replay-suspect-rewind");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_range(&writer, 0, 10, t0);
        let mut replay = MidSessionReplay::default();
        replay.observe_probe(
            ReplayProbe {
                clean_probes: 1,
                suspect: false,
            },
            t0,
        );
        let mut sink = ReplaySink::default();
        let mut now = t0 + 100;
        drive(
            &mut replay,
            &mut sink,
            &writer,
            &spill,
            &mut now,
            10,
            |_, sink| sink.committed.len() == 10,
        );
        now += SEAL_REPLAY_HEALTHY_SECS;
        replay.step(&mut sink, &writer, &spill, true, now);
        assert_eq!(replay.awaiting_archive.len(), 1);

        // The table turns suspect: every ack since the last clean probe may
        // have been a lie.
        let rewound = replay.observe_probe(
            ReplayProbe {
                clean_probes: 1,
                suspect: true,
            },
            now,
        );
        assert_eq!(rewound, 1);
        assert!(replay.awaiting_archive.is_empty());
        let while_suspect = replay.step(&mut sink, &writer, &spill, true, now + 600);
        assert!(
            while_suspect.is_idle(),
            "nothing replays into a suspect table"
        );

        // Healthy again: the file is read from its start.
        replay.observe_probe(
            ReplayProbe {
                clean_probes: 2,
                suspect: false,
            },
            now + 700,
        );
        now += 700;
        let again = drive(
            &mut replay,
            &mut sink,
            &writer,
            &spill,
            &mut now,
            10,
            |_, sink| sink.committed.len() == 20,
        );
        assert_eq!(again.seals_reingested, 10, "every record written again");
        let mut expected: Vec<u32> = (0..10).collect();
        expected.extend(0..10);
        assert_eq!(sink.committed, expected);
        assert_eq!(
            count_bin(&spill.join(SEAL_ARCHIVE_SUBDIR)),
            0,
            "still unconfirmed"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn observe_probe_rewinds_the_file_being_read_and_every_parked_file_on_suspicion() {
        let mut replay = MidSessionReplay {
            cursor: Some(ReplayCursor {
                offset: 4 * SEAL_SPILL_RECORD_SIZE as u64,
                ..ReplayCursor::new(PathBuf::from("a.bin"))
            }),
            ..MidSessionReplay::default()
        };
        replay.parked.push(ParkedFile {
            cursor: ReplayCursor {
                offset: 9 * SEAL_SPILL_RECORD_SIZE as u64,
                ..ReplayCursor::new(PathBuf::from("b.bin"))
            },
            retry_at: 0,
        });
        let suspect = ReplayProbe {
            clean_probes: 0,
            suspect: true,
        };
        assert_eq!(replay.observe_probe(suspect, 10), 2);
        assert_eq!(replay.cursor.as_ref().map(|c| c.offset), Some(0));
        assert_eq!(replay.parked[0].cursor.offset, 0);
        assert_eq!(
            replay.observe_probe(suspect, 11),
            0,
            "only the transition into suspicion rewinds"
        );
    }

    #[test]
    fn a_failed_replay_flush_halves_the_next_step_and_a_success_doubles_it() {
        let (spill, dlq) = temp_pair("replay-step-halving");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        let total = u32::try_from(SEAL_REPLAY_SEALS_PER_CYCLE * 4).expect("fits");
        spill_n(&writer, total, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink {
            fail_flushes: 1,
            ..ReplaySink::default()
        };
        replay.observe(&healthy_drain(), t0);
        assert!(
            replay
                .step(&mut sink, &writer, &spill, true, t0 + 60)
                .flush_failed
        );

        replay.observe(&healthy_drain(), t0 + 100);
        let half = replay.step(&mut sink, &writer, &spill, true, t0 + 160);
        assert_eq!(half.seals_reingested, SEAL_REPLAY_SEALS_PER_CYCLE / 2);
        let full = replay.step(&mut sink, &writer, &spill, true, t0 + 160);
        assert_eq!(full.seals_reingested, SEAL_REPLAY_SEALS_PER_CYCLE);
        let capped = replay.step(&mut sink, &writer, &spill, true, t0 + 160);
        assert_eq!(
            capped.seals_reingested, SEAL_REPLAY_SEALS_PER_CYCLE,
            "doubling never goes past the rate cap"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_database_that_keeps_failing_never_skips_a_record() {
        // Every flush fails: the step halves down to one record, and the
        // single-record failures are counted, but the gate resets on each so
        // the file is never given up on while the writer is not healthy.
        let (spill, dlq) = temp_pair("replay-never-skip-while-down");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 50, t0);
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink {
            fail_flushes: usize::MAX,
            ..ReplaySink::default()
        };
        replay.observe(&healthy_drain(), t0);
        let first = replay.step(&mut sink, &writer, &spill, true, t0 + 60);
        assert!(first.flush_failed);
        // The live writer is failing too: no healthy drain is observed, so
        // the replay never runs again, whatever the clock says.
        replay.observe(&failed_drain(), t0 + 61);
        for minute in 2..30 {
            let step = replay.step(&mut sink, &writer, &spill, true, t0 + minute * 60);
            assert!(step.is_idle(), "a failing database is never fed a backlog");
        }
        assert_eq!(sink.flushes, 1);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn test_is_healthy_needs_a_clean_flush_and_sixty_seconds() {
        let t0 = jan1_noon_utc();
        let mut replay = MidSessionReplay::default();
        assert!(!replay.is_healthy(t0), "no flush seen yet");
        replay.observe(&healthy_drain(), t0);
        assert!(!replay.is_healthy(t0 + SEAL_REPLAY_HEALTHY_SECS - 1));
        assert!(replay.is_healthy(t0 + SEAL_REPLAY_HEALTHY_SECS));
        replay.observe(&failed_drain(), t0 + SEAL_REPLAY_HEALTHY_SECS);
        assert!(!replay.is_healthy(t0 + 10 * SEAL_REPLAY_HEALTHY_SECS));
    }

    #[test]
    fn a_clock_that_steps_backwards_restarts_the_gate_and_makes_a_scan_due() {
        let t0 = jan1_noon_utc();
        let mut replay = MidSessionReplay::default();
        replay.observe(&healthy_drain(), t0);
        assert!(replay.is_healthy(t0 + 60));
        // The clock jumps back an hour.
        let back = t0 - 3_600;
        replay.observe(&healthy_drain(), back);
        assert!(!replay.is_healthy(back + 59), "never opens early");
        assert!(
            replay.is_healthy(back + 60),
            "reopens after sixty seconds of the new clock, not after an hour"
        );

        let (spill, dlq) = temp_pair("replay-clock-back");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let mut sink = ReplaySink::default();
        // A scan at t0 finds nothing and records the scan time.
        replay.observe(&healthy_drain(), t0);
        assert!(
            replay
                .step(&mut sink, &writer, &spill, true, t0 + 60)
                .is_idle()
        );
        spill_n(&writer, 3, t0);
        // Back in time: the scan is due at once rather than in an hour.
        replay.observe(&healthy_drain(), back);
        let step = replay.step(&mut sink, &writer, &spill, true, back + 60);
        assert_eq!(step.seals_reingested, 3);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn replay_skips_records_from_another_format_version_and_still_archives() {
        let (spill, dlq) = temp_pair("replay-version-gate");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 3, t0);
        // Append a record claiming an older format, then two good ones.
        let path = writer.spill_path(t0);
        let mut stale = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, 99, 1.0)).to_bytes();
        stale[7] = crate::seal_spill::SEAL_SPILL_OLDEST_READABLE_VERSION.wrapping_sub(1);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            f.write_all(&stale).expect("write");
        }
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);
        let out = replay.step(&mut sink, &writer, &spill, true, t0 + 60);
        assert_eq!(out.seals_reingested, 3);
        assert_eq!(out.records_skipped, 1);
        assert_eq!(out.files_archived, 1);
        cleanup(&spill, &dlq);
    }

    /// Audit PR41c: the mid-session replay skips a damaged record, counts it,
    /// and re-ingests the records on both sides of it.
    #[test]
    fn replay_skips_a_damaged_record_and_reingests_the_rest() {
        let (spill, dlq) = temp_pair("replay-damaged");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 3, t0);
        let path = writer.spill_path(t0);
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[SEAL_SPILL_RECORD_SIZE + 64] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);
        let out = replay.step(&mut sink, &writer, &spill, true, t0 + 60);
        assert_eq!(out.seals_reingested, 2);
        assert_eq!(out.records_skipped, 1);
        assert_eq!(out.files_archived, 1);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn replay_treats_a_torn_tail_as_the_end_of_the_file() {
        // Nothing appends to a staged file, so a short tail can only be a
        // write that never completed: the whole records before it are
        // re-ingested and the file is archived.
        let (spill, dlq) = temp_pair("replay-torn-tail");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 3, t0);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(writer.spill_path(t0))
                .expect("open");
            f.write_all(&[9u8; 50]).expect("torn bytes");
        }
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);
        let out = replay.step(&mut sink, &writer, &spill, true, t0 + 60);
        assert_eq!(out.seals_reingested, 3);
        assert_eq!(out.files_archived, 1);
        assert_eq!(sink.committed, vec![0, 1, 2]);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn replay_scans_at_most_once_per_interval_while_idle() {
        let (spill, dlq) = temp_pair("replay-scan-interval");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        let mut replay = MidSessionReplay::default();
        let mut sink = ReplaySink::default();
        replay.observe(&healthy_drain(), t0);
        // First scan finds nothing.
        assert!(
            replay
                .step(&mut sink, &writer, &spill, true, t0 + 60)
                .is_idle()
        );
        spill_n(&writer, 4, t0);
        assert!(
            replay
                .step(
                    &mut sink,
                    &writer,
                    &spill,
                    true,
                    t0 + 60 + SEAL_REPLAY_SCAN_EVERY_SECS - 1
                )
                .is_idle(),
            "the directory is not re-read inside the scan interval"
        );
        let out = replay.step(
            &mut sink,
            &writer,
            &spill,
            true,
            t0 + 60 + SEAL_REPLAY_SCAN_EVERY_SECS,
        );
        assert_eq!(out.seals_reingested, 4);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn staging_the_live_file_while_appends_race_loses_no_seal() {
        // The no-loss contract of `with_appends_paused`: an appender racing
        // the stage lands every record either in a staged file or in the
        // fresh live file, never in a moved inode nothing reads.
        let (spill, dlq) = temp_pair("replay-stage-race");
        let writer = std::sync::Arc::new(
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone()),
        );
        let t0 = jan1_noon_utc();
        const TOTAL: u32 = 20_000;
        let appender = {
            let writer = std::sync::Arc::clone(&writer);
            std::thread::spawn(move || spill_n(&writer, TOTAL, t0))
        };
        let mut staged = 0usize;
        while !appender.is_finished() {
            staged += stage_live_spill_files(&writer, &spill);
        }
        appender.join().expect("appender must not panic");
        staged += stage_live_spill_files(&writer, &spill);
        assert!(staged >= 1);

        let replaying = spill.join(SEAL_REPLAYING_SUBDIR);
        let mut bytes = 0u64;
        for entry in std::fs::read_dir(&replaying)
            .expect("staging dir")
            .flatten()
        {
            let len = entry.metadata().expect("meta").len();
            assert_eq!(len % SEAL_SPILL_RECORD_SIZE as u64, 0, "no torn record");
            bytes += len;
        }
        assert_eq!(count_bin(&spill), 0, "everything was staged");
        assert_eq!(
            bytes,
            u64::from(TOTAL) * SEAL_SPILL_RECORD_SIZE as u64,
            "every appended seal must be in a staged file"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn staging_with_appends_paused_sends_the_next_seal_to_a_fresh_live_file() {
        // Audit PR40c. The race test above cannot fail when the pause is
        // removed: its appender almost never holds the lock at the instant of
        // a rename. This one is deterministic. The first appends leave the
        // writer holding an open handle on the live file; staging moves that
        // file. The pause drops the handle, so the next append opens a fresh
        // live file. If `with_appends_paused` ran its closure without closing
        // the handle, the next seal would go into the MOVED inode, a file the
        // replay may already have read and archived, and nothing would ever
        // read it again. Verified by hand on 2026-09-27: with the `*open = None`
        // line removed from `with_appends_paused`, this test fails on the first
        // assertion below while the race test above still passes.
        let (spill, dlq) = temp_pair("replay-stage-pause");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        spill_n(&writer, 3, t0);
        assert_eq!(stage_live_spill_files(&writer, &spill), 1);
        assert_eq!(count_bin(&spill), 0, "the live file was staged");

        let late = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, 7, 101.5));
        writer.append_seal(&late, t0).expect("spill append");

        let live_len = std::fs::metadata(writer.spill_path(t0))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(
            live_len, SEAL_SPILL_RECORD_SIZE as u64,
            "a seal appended after staging must open a fresh live file"
        );
        let replaying = spill.join(SEAL_REPLAYING_SUBDIR);
        let staged: Vec<u64> = std::fs::read_dir(&replaying)
            .expect("staging dir")
            .flatten()
            .map(|e| e.metadata().expect("meta").len())
            .collect();
        assert_eq!(
            staged,
            vec![3 * SEAL_SPILL_RECORD_SIZE as u64],
            "the staged file keeps exactly the seals written before the pause"
        );
        cleanup(&spill, &dlq);
    }

    // -----------------------------------------------------------------------
    // Audit PR41a — a replayed candle never replaces a fuller one
    // -----------------------------------------------------------------------

    /// A sink that records `(bucket, tick_count)` of what it committed.
    #[derive(Default)]
    struct CopySink {
        pending: Vec<(u32, u32)>,
        committed: Vec<(u32, u32)>,
    }

    impl SealSink for CopySink {
        fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
            self.pending
                .push((seal.state.bucket_start_ist_secs, seal.state.tick_count));
            Ok(())
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            self.committed.append(&mut self.pending);
            Ok(())
        }
        fn discard_pending(&mut self) {
            self.pending.clear();
        }
    }

    fn copy_with_ticks(bucket: u32, ticks: u32) -> BufferedSeal {
        let mut seal = mk_seal(13, 2, TfIndex::M1, bucket, 101.5);
        seal.state.tick_count = ticks;
        seal
    }

    #[test]
    fn pr41a_mid_session_replay_writes_only_the_fuller_copy() {
        let (spill, dlq) = temp_pair("pr41a-replay");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        // The original spills (ring full, database up), then the amended copy
        // commits live and the writer appends it behind the original.
        let original = copy_with_ticks(600, 4);
        let amended = copy_with_ticks(600, 5);
        writer
            .append_seal(&SerializedSeal::from(&original), t0)
            .expect("spill original");
        assert_eq!(writer.note_live_commits(&[amended], t0), 1);

        let mut replay = MidSessionReplay::default();
        let mut sink = CopySink::default();
        replay.observe(&healthy_drain(), t0);
        let out = replay.step(
            &mut sink,
            &writer,
            &spill,
            true,
            t0 + SEAL_REPLAY_HEALTHY_SECS,
        );
        assert_eq!(out.files_archived, 1);
        assert_eq!(out.seals_reingested, 1);
        assert_eq!(out.records_skipped, 0, "a superseded copy is not a loss");
        assert_eq!(
            sink.committed,
            vec![(600, 5)],
            "the original is never written"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn pr41a_boot_drain_never_writes_an_older_copy_after_a_fuller_one() {
        let (spill, dlq) = temp_pair("pr41a-boot");
        let t0 = jan1_noon_utc();
        // The fuller copy is in the spill; an older copy sits in a dead-letter
        // file written later (the shape the file order cannot fix).
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        spill_writer
            .append_seal(&SerializedSeal::from(&copy_with_ticks(600, 5)), t0)
            .expect("spill fuller");
        let dlq_writer = crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(dlq.clone());
        dlq_writer
            .append_record(
                &SealDlqRecord::from(&SerializedSeal::from(&copy_with_ticks(600, 4))),
                t0,
            )
            .expect("dlq older");

        let mut sink = CopySink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(outcome.seals_recovered, 2);
        assert_eq!(outcome.seals_reingested, 1);
        assert_eq!(outcome.seals_superseded, 1);
        assert_eq!(
            outcome.files_archived, 2,
            "a superseded copy does not hold its file"
        );
        assert_eq!(sink.committed, vec![(600, 5)]);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn pr41a_boot_drain_writes_both_copies_when_the_older_comes_first() {
        let (spill, dlq) = temp_pair("pr41a-boot-order");
        let t0 = jan1_noon_utc();
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        // A fresh writer has no ledger, so both copies land in file order.
        for seal in [
            copy_with_ticks(600, 4),
            copy_with_ticks(600, 5),
            copy_with_ticks(660, 1),
        ] {
            spill_writer
                .append_seal(&SerializedSeal::from(&seal), t0)
                .expect("spill");
        }
        let mut sink = CopySink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(outcome.seals_superseded, 0);
        // The fuller copy is written last, so it is the one the table keeps.
        assert_eq!(sink.committed, vec![(600, 4), (600, 5), (660, 1)]);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn pr41a_staged_files_replay_oldest_write_first() {
        let (spill, dlq) = temp_pair("pr41a-write-order");
        // Name order puts the day file before its set-aside part, although
        // the set-aside part was written first.
        let day = spill.join("seals_v4-2026-10-01.bin");
        let aside = spill.join("seals_v4-2026-10-01.bin.1");
        std::fs::write(&day, b"").expect("day file");
        std::fs::write(&aside, b"").expect("aside file");
        let earlier =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_759_300_000);
        let later = earlier + std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&aside)
            .and_then(|f| f.set_modified(earlier))
            .expect("aside mtime");
        std::fs::File::options()
            .write(true)
            .open(&day)
            .and_then(|f| f.set_modified(later))
            .expect("day mtime");
        let mut files = vec![day.clone(), aside.clone()];
        sort_by_write_time(&mut files);
        assert_eq!(files, vec![aside, day]);
        cleanup(&spill, &dlq);
    }

    // -----------------------------------------------------------------------
    // Z6 — the fullest copy of a bar wins, whatever order its copies replay in
    // -----------------------------------------------------------------------

    /// The tick count of the LAST row the sink committed for `bucket`: the
    /// row the candle table keeps under its last-write-wins DEDUP key.
    fn z6_final_ticks(committed: &[(u32, u32)], bucket: u32) -> Option<u32> {
        committed
            .iter()
            .rev()
            .find(|(b, _)| *b == bucket)
            .map(|(_, ticks)| *ticks)
    }

    fn z6_spill(writer: &crate::seal_spill::SealSpillWriter, seal: &BufferedSeal, now: i64) {
        writer
            .append_seal(&SerializedSeal::from(seal), now)
            .expect("spill append");
    }

    fn z6_dead_letter(dlq: &Path, seal: &BufferedSeal, now: i64) {
        crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(dlq.to_path_buf())
            .append_record(&SealDlqRecord::from(&SerializedSeal::from(seal)), now)
            .expect("dlq append");
    }

    /// Path A: the original spilled, then a LATER bucket of the same slot
    /// spilled, then the amend committed live. The old one-entry-per-slot
    /// ledger had replaced the original's entry with the later bucket's, so
    /// the amend was never mirrored and the boot drain ended on the original.
    #[test]
    fn test_regression_z6_later_bucket_spilled_does_not_hide_amend() {
        let (spill, dlq) = temp_pair("z6-path-a");
        let t0 = jan1_noon_utc();
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        z6_spill(&writer, &copy_with_ticks(600, 4), t0);
        z6_spill(&writer, &copy_with_ticks(660, 1), t0);
        assert_eq!(
            writer.note_live_commits(&[copy_with_ticks(600, 5)], t0),
            1,
            "the amend of the earlier bar must be mirrored behind its original"
        );
        drop(writer);

        let mut sink = CopySink::default();
        drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(z6_final_ticks(&sink.committed, 600), Some(5));
        assert_eq!(z6_final_ticks(&sink.committed, 660), Some(1));
        cleanup(&spill, &dlq);
    }

    /// Path C at boot: the spill and the dead-letter file each hold a copy of
    /// one bar, with a later bucket of the same slot between them. Either way
    /// round, the drain's final row is the fuller copy.
    #[test]
    fn test_regression_z6_boot_drain_keeps_fullest_across_spill_and_dlq_with_intervening_bucket() {
        for (spilled, dead_lettered) in [(5, 4), (4, 5)] {
            let (spill, dlq) = temp_pair(&format!("z6-path-c-boot-{spilled}"));
            let t0 = jan1_noon_utc();
            let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
            z6_spill(&writer, &copy_with_ticks(600, spilled), t0);
            z6_spill(&writer, &copy_with_ticks(660, 1), t0);
            drop(writer);
            z6_dead_letter(&dlq, &copy_with_ticks(600, dead_lettered), t0);

            let mut sink = CopySink::default();
            let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
            assert_eq!(
                z6_final_ticks(&sink.committed, 600),
                Some(5),
                "spill copy {spilled}, dead-letter copy {dead_lettered}: {:?}",
                sink.committed
            );
            assert_eq!(
                z6_final_ticks(&sink.committed, 660),
                Some(1),
                "the later bucket between them is its own bar and is written"
            );
            assert_eq!(outcome.seals_untracked, 0);
            cleanup(&spill, &dlq);
        }
    }

    // -----------------------------------------------------------------------
    // S1 — the older-copy guard survives a drain that stops on a dead database
    // -----------------------------------------------------------------------

    /// A sink recording `(bucket, ticks)` that lets `ok_flushes` flushes
    /// land and fails every one after (the database dies mid-drain).
    struct S1DyingSink {
        pending: Vec<(u32, u32)>,
        committed: Vec<(u32, u32)>,
        ok_flushes: usize,
    }

    impl SealSink for S1DyingSink {
        fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
            self.pending
                .push((seal.state.bucket_start_ist_secs, seal.state.tick_count));
            Ok(())
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            if self.ok_flushes == 0 {
                anyhow::bail!("connection refused");
            }
            self.ok_flushes -= 1;
            self.committed.append(&mut self.pending);
            Ok(())
        }
        fn discard_pending(&mut self) {
            self.pending.clear();
        }
    }

    /// Boot 1 commits the 5-tick copy from a spill file and archives it, then
    /// the database dies on the dead-letter file, which also holds a 4-tick
    /// copy of the same bar. Boot 2 must not write the 4-tick copy over the
    /// 5-tick row, although it never re-reads the archived spill file.
    #[test]
    fn test_regression_s1_older_copy_left_staged_by_a_stopped_drain_is_refused_next_boot() {
        let (spill, dlq) = temp_pair("s1-two-boots");
        let t0 = jan1_noon_utc();
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        z6_spill(&writer, &copy_with_ticks(600, 5), t0);
        drop(writer);
        let dlq_writer = crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(dlq.clone());
        for seal in [copy_with_ticks(600, 4), copy_with_ticks(720, 1)] {
            dlq_writer
                .append_record(&SealDlqRecord::from(&SerializedSeal::from(&seal)), t0)
                .expect("dlq append");
        }
        drop(dlq_writer);

        let mut boot1 = S1DyingSink {
            pending: Vec::new(),
            committed: Vec::new(),
            ok_flushes: 1,
        };
        let first = drain_recovered_seals(&mut boot1, &spill, &dlq, 64);
        assert_eq!(boot1.committed, vec![(600, 5)]);
        assert_eq!(first.files_archived, 1, "the spill file is archived");
        assert_eq!(
            first.files_left_pending, 1,
            "the dead-letter file stays staged"
        );
        assert_eq!(first.summary_not_persisted, 0);
        assert!(
            spill.join(SEAL_BOOT_SUMMARY_FILE).is_file(),
            "a stopped drain leaves its summary"
        );

        let mut boot2 = CopySink::default();
        let second = drain_recovered_seals(&mut boot2, &spill, &dlq, 64);
        assert_eq!(second.summary_seeded, 1);
        assert_eq!(second.seals_superseded, 1, "the refusal is counted");
        assert_eq!(
            boot2.committed,
            vec![(720, 1)],
            "the 4-tick copy never replaces the 5-tick row"
        );
        assert_eq!(second.files_left_pending, 0);
        assert!(
            !spill.join(SEAL_BOOT_SUMMARY_FILE).exists(),
            "a drain that finishes removes the summary"
        );
        cleanup(&spill, &dlq);
    }

    /// The guard is carried through more than one stopped drain: boot 2 also
    /// dies, and boot 3 still refuses the older copy.
    #[test]
    fn test_regression_s1_summary_survives_two_stopped_drains() {
        let (spill, dlq) = temp_pair("s1-three-boots");
        let t0 = jan1_noon_utc();
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        z6_spill(&writer, &copy_with_ticks(600, 5), t0);
        drop(writer);
        // First dead-letter file: the one each dying boot stops on.
        z6_dead_letter(&dlq, &copy_with_ticks(720, 1), t0);
        let first = dlq.join("seals_v4-2026-01-01-a.ndjson");
        std::fs::rename(dlq.join("seals_v4-2026-01-01.ndjson"), &first).expect("rename");
        // Second dead-letter file, behind it: the older copy no dying boot reaches.
        z6_dead_letter(&dlq, &copy_with_ticks(600, 4), t0);
        let second = dlq.join("seals_v4-2026-01-01.ndjson");
        let earlier =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_759_300_000);
        for (path, offset) in [(&first, 0u64), (&second, 60)] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|f| f.set_modified(earlier + std::time::Duration::from_secs(offset)))
                .expect("mtime");
        }

        let mut boot1 = S1DyingSink {
            pending: Vec::new(),
            committed: Vec::new(),
            ok_flushes: 1,
        };
        drain_recovered_seals(&mut boot1, &spill, &dlq, 64);
        assert_eq!(boot1.committed, vec![(600, 5)]);

        let mut boot2 = S1DyingSink {
            pending: Vec::new(),
            committed: Vec::new(),
            ok_flushes: 0,
        };
        let second = drain_recovered_seals(&mut boot2, &spill, &dlq, 64);
        assert!(second.files_left_pending > 0);
        assert!(spill.join(SEAL_BOOT_SUMMARY_FILE).is_file());

        let mut boot3 = CopySink::default();
        let third = drain_recovered_seals(&mut boot3, &spill, &dlq, 64);
        assert_eq!(third.summary_seeded, 1);
        assert_eq!(z6_final_ticks(&boot3.committed, 600), None);
        assert_eq!(z6_final_ticks(&boot3.committed, 720), Some(1));
        cleanup(&spill, &dlq);
    }

    /// A damaged summary is refused whole and counted; the drain still runs.
    #[test]
    fn test_regression_s1_damaged_summary_is_refused_and_the_drain_still_runs() {
        let (spill, dlq) = temp_pair("s1-damaged");
        let t0 = jan1_noon_utc();
        std::fs::create_dir_all(&spill).expect("spill dir");
        std::fs::write(spill.join(SEAL_BOOT_SUMMARY_FILE), b"garbage").expect("summary");
        z6_dead_letter(&dlq, &copy_with_ticks(600, 4), t0);
        let mut sink = CopySink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(outcome.summary_refused, 1);
        assert_eq!(outcome.summary_seeded, 0);
        assert_eq!(sink.committed, vec![(600, 4)]);
        assert!(!spill.join(SEAL_BOOT_SUMMARY_FILE).exists());
        cleanup(&spill, &dlq);
    }

    /// A drain with nothing staged removes a leftover summary.
    #[test]
    fn test_regression_s1_nothing_staged_removes_a_leftover_summary() {
        let (spill, dlq) = temp_pair("s1-empty");
        std::fs::create_dir_all(&spill).expect("spill dir");
        std::fs::write(spill.join(SEAL_BOOT_SUMMARY_FILE), b"x").expect("summary");
        let mut sink = CopySink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert!(outcome.is_clean());
        assert!(!spill.join(SEAL_BOOT_SUMMARY_FILE).exists());
        cleanup(&spill, &dlq);
    }

    /// A sink recording `(bucket, ticks)` whose flushes die on the
    /// connection while they carry the `poison` bucket, so the replay parks
    /// the file at that record (audit PR41b).
    #[derive(Default)]
    struct Z6ParkingSink {
        pending: Vec<(u32, u32)>,
        committed: Vec<(u32, u32)>,
        poison: Option<u32>,
    }

    impl SealSink for Z6ParkingSink {
        fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
            self.pending
                .push((seal.state.bucket_start_ist_secs, seal.state.tick_count));
            Ok(())
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            if self
                .poison
                .is_some_and(|p| self.pending.iter().any(|(b, _)| *b == p))
            {
                return Err(anyhow::Error::new(questdb::Error::new(
                    questdb::ErrorCode::SocketError,
                    "injected: connection reset",
                ))
                .context("shadow flush: ILP send failed after reconnect retries"));
            }
            self.committed.append(&mut self.pending);
            Ok(())
        }
        fn discard_pending(&mut self) {
            self.pending.clear();
        }
    }

    /// Path D: file F1 holds the original behind a record that cannot flush,
    /// so F1 is parked; file F2 holds the amend and a later bucket of the
    /// same slot, and replays first. When F1 resumes, the original must be
    /// dropped: the old ledger compared it with the later bucket and wrote it
    /// over the amend.
    #[test]
    fn test_regression_z6_parked_file_resumed_after_later_bucket_skips_older_copy() {
        let (spill, dlq) = temp_pair("z6-path-d");
        let writer = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let t0 = jan1_noon_utc();
        // F1 (day t0): a stuck record, then the original.
        z6_spill(&writer, &copy_with_ticks(540, 1), t0);
        z6_spill(&writer, &copy_with_ticks(600, 4), t0);
        // F2 (the next day's file): the amend, then a later bucket.
        z6_spill(&writer, &copy_with_ticks(600, 5), t0 + 86_400);
        z6_spill(&writer, &copy_with_ticks(660, 1), t0 + 86_400);

        let mut replay = MidSessionReplay::default();
        let mut sink = Z6ParkingSink {
            poison: Some(540),
            ..Z6ParkingSink::default()
        };
        let mut now = t0 + 2 * 86_400;
        replay.observe(&healthy_drain(), now);
        for _ in 0..200 {
            now += SEAL_REPLAY_HEALTHY_SECS;
            let step = replay.step(&mut sink, &writer, &spill, true, now);
            if step.flush_failed {
                replay.observe(&healthy_drain(), now);
            }
            if sink.committed.iter().any(|(b, _)| *b == 660) {
                break;
            }
        }
        assert_eq!(replay.parked.len(), 1, "F1 is parked at the stuck record");
        assert_eq!(z6_final_ticks(&sink.committed, 600), Some(5));

        // The connection heals and F1 resumes after its park window.
        sink.poison = None;
        now += SEAL_REPLAY_PARK_SECS;
        replay.observe(&healthy_drain(), now);
        // Driven until F1 is read to its end and archived, so the original
        // behind the stuck record is certainly read back (the step size is
        // down to one record after the failures, so committing the stuck
        // record alone does not mean the original was reached).
        let mut archived = 0usize;
        for _ in 0..50 {
            now += SEAL_REPLAY_HEALTHY_SECS;
            archived += replay
                .step(&mut sink, &writer, &spill, true, now)
                .files_archived;
            if archived > 0 {
                break;
            }
        }
        assert_eq!(archived, 1, "F1 resumed and was read to its end");
        assert!(replay.parked.is_empty());
        assert!(
            sink.committed.contains(&(540, 1)),
            "F1 resumed: {:?}",
            sink.committed
        );
        assert_eq!(
            z6_final_ticks(&sink.committed, 600),
            Some(5),
            "the resumed file must not write the original over the amend: {:?}",
            sink.committed
        );
        assert!(!sink.committed.contains(&(600, 4)));
        cleanup(&spill, &dlq);
    }

    /// Where one copy of a bar is written before the boot drain reads it.
    #[derive(Clone, Copy, Debug)]
    enum Z6Tier {
        Spill,
        DeadLetter,
    }

    static Z6_PROPTEST_CASE: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// Any mix of copies of two bars of one slot, written to the spill
        /// and the dead-letter file in any order: the boot drain's final row
        /// for each bar is its fullest copy.
        #[test]
        fn z6_any_replay_order_ends_on_fullest_copy(
            copies in proptest::collection::vec(
                (
                    proptest::prelude::prop_oneof![
                        proptest::prelude::Just(600_u32),
                        proptest::prelude::Just(660_u32),
                    ],
                    1_u32..=6,
                    proptest::prelude::prop_oneof![
                        proptest::prelude::Just(Z6Tier::Spill),
                        proptest::prelude::Just(Z6Tier::DeadLetter),
                    ],
                ),
                1..12,
            )
        ) {
            let case = Z6_PROPTEST_CASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (spill, dlq) = temp_pair(&format!("z6-proptest-{case}"));
            let t0 = jan1_noon_utc();
            let writer =
                crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
            for (bucket, ticks, tier) in &copies {
                let seal = copy_with_ticks(*bucket, *ticks);
                match tier {
                    Z6Tier::Spill => z6_spill(&writer, &seal, t0),
                    Z6Tier::DeadLetter => z6_dead_letter(&dlq, &seal, t0),
                }
            }
            drop(writer);

            let mut sink = CopySink::default();
            drain_recovered_seals(&mut sink, &spill, &dlq, 64);
            cleanup(&spill, &dlq);
            for bucket in [600_u32, 660] {
                let fullest = copies
                    .iter()
                    .filter(|(b, _, _)| *b == bucket)
                    .map(|(_, ticks, _)| *ticks)
                    .max();
                proptest::prop_assert_eq!(
                    z6_final_ticks(&sink.committed, bucket),
                    fullest,
                    "bucket {}: {:?} from {:?}",
                    bucket,
                    &sink.committed,
                    &copies
                );
            }
        }
    }

    #[test]
    fn staging_reads_overflow_and_twice_renamed_copies() {
        // PR40b-f: only one `.N` suffix was split, so a `.bin.overflow` file
        // (free_path's last resort) or a copy renamed twice (`.bin.1.1`) was
        // never staged, read or replayed.
        for name in [
            "seals_v4-2026-10-01.bin.overflow",
            "seals_v4-2026-10-01.bin.1.1",
        ] {
            assert!(
                matches!(staged_kind(Path::new(name)), Some(StagedKind::Spill)),
                "{name} must classify as a spill file"
            );
        }
        let dir = std::env::temp_dir().join(format!(
            "tv-seal-stage-overflow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("seals_v4-2026-10-01.bin.overflow"), b"").expect("write");
        let staged = stage_pending_files(&dir);
        assert_eq!(
            staged.len(),
            1,
            "the overflow file must be staged: {staged:?}"
        );
        assert!(staged[0].starts_with(dir.join(SEAL_REPLAYING_SUBDIR)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_boot_drain_lets_the_retention_sweep_delete_unreplayed_files() {
        // PR40b-f: the sweep holds aged unreplayed files until the boot drain
        // has read the folder, whatever the drain found.
        let dir = std::env::temp_dir().join(format!(
            "tv-seal-drain-marks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!crate::seal_spill::boot_drain_ran(&dir));
        let mut sink = ReplaySink::default();
        let _ = drain_recovered_seals(&mut sink, &dir, &dir, 8);
        assert!(crate::seal_spill::boot_drain_ran(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
