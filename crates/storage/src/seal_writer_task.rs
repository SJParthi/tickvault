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
use crate::seal_spill::{SEAL_SPILL_FORMAT_VERSION, SEAL_SPILL_RECORD_SIZE, SerializedSeal};
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
    // leftovers. Sorted so recovery is deterministic (oldest date first).
    let mut staged: Vec<PathBuf> = std::fs::read_dir(&replaying)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_seal_file(p))
        .collect();
    staged.sort();
    staged
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
    // Split off any `.N` collision suffix before classifying.
    let base = name.rsplit_once('.').map_or(name, |(head, tail)| {
        if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() {
            head
        } else {
            name
        }
    });
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
        if buf[7] != SEAL_SPILL_FORMAT_VERSION {
            undecodable += 1;
            continue;
        }
        match SerializedSeal::from_bytes(&buf) {
            Some(seal) => out.push(seal),
            None => undecodable += 1,
        }
    }
    Some((out, undecodable))
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
            Ok(record) if record.format_version != SEAL_SPILL_FORMAT_VERSION => {
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
    let mut outcome = BootDrainOutcome::default();
    let batch = max_batch.max(1);

    let mut staged = stage_pending_files(spill_dir);
    if dlq_dir != spill_dir {
        staged.extend(stage_pending_files(dlq_dir));
    }
    outcome.files_staged = staged.len();
    if staged.is_empty() {
        return outcome;
    }

    info!(
        files = staged.len(),
        "seal recovery: replaying orphaned spill/DLQ files from disk"
    );

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
                appended += 1;
            }
            if appended == 0 {
                continue;
            }
            match writer.flush() {
                Ok(()) => committed += appended,
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
                    file_ok = false;
                    break;
                }
            }
        }

        outcome.seals_reingested += committed;
        outcome.seals_append_failed += append_failed;

        if file_ok && append_failed > 0 {
            // The database is up (every flush landed), so the remaining files
            // are still worth trying: no `halted`. Re-reading this file on the
            // next boot re-sends the seals that did land, and the DEDUP key
            // collapses them.
            outcome.files_left_pending += 1;
            outcome.seals_left_pending += append_failed;
        } else if file_ok {
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

    if outcome.files_left_pending > 0 {
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            files_pending = outcome.files_left_pending,
            seals_pending = outcome.seals_left_pending,
            seals_append_failed = outcome.seals_append_failed,
            seals_reingested = outcome.seals_reingested,
            "seal recovery INCOMPLETE — sealed candles are on DISK and NOT in QuestDB"
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
             or replaying/) for inspection — this drain deletes nothing",
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
// * **Last write wins.** A replayed seal UPSERTs over whatever row the tables
//   hold for its key, exactly as the boot drain always has. If a later,
//   amended seal for the same bar was written live before the replay reached
//   the older one, the older one overwrites it.
// * **The gate needs live traffic.** It opens only on clean LIVE flushes. If
//   the last flush before the close failed, no live seal arrives afterwards
//   to reopen it, and the spill waits for the next boot's drain.
// * **Clock steps.** A wall clock that steps backwards restarts the gate
//   (never opens it early), and makes a directory scan due at once.
//
// Only the binary spill is replayed here. The NDJSON DLQ is the last-resort
// tier (spill itself failed) and stays with the boot drain.

/// How long the live writer must flush cleanly before a replay step may run.
pub const SEAL_REPLAY_HEALTHY_SECS: i64 = 60;

/// Most spilled seals re-ingested per writer cycle (every 100 ms).
pub const SEAL_REPLAY_SEALS_PER_CYCLE: usize = 512;

/// Consecutive replay flush failures at a step of one record, at the same
/// position, before that record is skipped as poison.
const SEAL_REPLAY_POISON_FAILURES: u32 = 2;

/// How often, while idle and healthy, the replay looks for spill files.
pub const SEAL_REPLAY_SCAN_EVERY_SECS: i64 = 30;

/// Files the replay gave up on for this process (unreadable, or the archive
/// rename failed). Bounded so a directory of broken files cannot grow it; past
/// the bound the replay stops looking for new files until the next boot.
const SEAL_REPLAY_SKIP_LIMIT: usize = 64;

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
    /// Consecutive failed flushes at a step of one record, at `offset`.
    single_record_failures: u32,
}

impl ReplayCursor {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            step_records: SEAL_REPLAY_SEALS_PER_CYCLE,
            single_record_failures: 0,
        }
    }
}

/// Mid-session replay of the seal spill. Owned by the seal writer runner and
/// stepped once per writer cycle, after the live drain.
///
/// Cold path: runs on the writer loop inside `block_in_place`, never on the
/// frame drain. Each step opens the staged file, reads at most
/// [`SEAL_REPLAY_SEALS_PER_CYCLE`] records into a reused buffer and flushes
/// them once: O(records) per step, bounded by the cap.
#[derive(Debug, Default)]
pub struct MidSessionReplay {
    /// Wall-clock second of the first clean live flush since the last failure.
    healthy_since: Option<i64>,
    /// Wall-clock second of the last directory scan.
    last_scan: Option<i64>,
    cursor: Option<ReplayCursor>,
    /// Files given up on for this process. See [`SEAL_REPLAY_SKIP_LIMIT`].
    skipped: Vec<PathBuf>,
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
            self.healthy_since = None;
        } else if drain.flushed_ok && self.healthy_since.is_none() {
            self.healthy_since = Some(now_unix_secs);
        }
    }

    /// `true` once the live writer has flushed cleanly for
    /// [`SEAL_REPLAY_HEALTHY_SECS`] with no failure since.
    #[must_use]
    pub fn is_healthy(&self, now_unix_secs: i64) -> bool {
        self.healthy_since
            .is_some_and(|since| now_unix_secs.saturating_sub(since) >= SEAL_REPLAY_HEALTHY_SECS)
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
            let Some(path) = self.next_staged_file(spill_dir) else {
                return outcome;
            };
            info!(
                ?path,
                files_staged = outcome.files_staged,
                "seal replay: re-ingesting a spill file mid-session"
            );
            self.cursor = Some(ReplayCursor::new(path));
        }
        self.advance(writer, &mut outcome);
        outcome
    }

    /// Oldest staged spill file not yet given up on.
    fn next_staged_file(&self, spill_dir: &Path) -> Option<PathBuf> {
        let replaying = spill_dir.join(SEAL_REPLAYING_SUBDIR);
        // O(1) EXEMPT: cold directory listing, at most once per scan interval
        let mut staged: Vec<PathBuf> = std::fs::read_dir(&replaying)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_seal_file(p) && staged_kind(p) == Some(StagedKind::Spill))
            .filter(|p| !self.skipped.contains(p))
            .collect();
        staged.sort();
        staged.into_iter().next()
    }

    /// Give up on the current file for this process. The next boot's drain
    /// retries it.
    fn skip_current(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            self.skipped.push(cursor.path);
            if self.skipped.len() == SEAL_REPLAY_SKIP_LIMIT {
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    files_skipped = SEAL_REPLAY_SKIP_LIMIT,
                    "seal replay: gave up on too many spill files — the replay stops for \
                     this process and the next boot's drain retries them"
                );
            }
        }
        // Scan again at once: another file may be waiting.
        self.last_scan = None;
    }

    /// Replay up to one step's worth of records from the cursor.
    fn advance<S: SealSink>(&mut self, writer: &mut S, outcome: &mut ReplayOutcome) {
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

        let mut appended = 0usize;
        for seal in &self.batch {
            if let Err(append_err) = writer.append_seal(seal) {
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    ?append_err,
                    security_id = seal.security_id,
                    "seal replay: append failed for a spilled seal"
                );
                outcome.records_skipped += 1;
                continue;
            }
            appended += 1;
        }
        if appended > 0 {
            if let Err(flush_err) = writer.flush() {
                writer.discard_pending();
                outcome.flush_failed = true;
                self.healthy_since = None;
                self.on_replay_flush_failed(&path, offset, &flush_err, outcome);
                return;
            }
            outcome.seals_reingested += appended;
        }

        let consumed =
            u64::try_from(records_read.saturating_mul(SEAL_SPILL_RECORD_SIZE)).unwrap_or(u64::MAX);
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.offset = cursor.offset.saturating_add(consumed);
            cursor.single_record_failures = 0;
            cursor.step_records = cursor
                .step_records
                .saturating_mul(2)
                .min(SEAL_REPLAY_SEALS_PER_CYCLE);
        }
        if at_eof {
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
    }

    /// A replay flush failed: the file keeps its position and the next step
    /// reads half as many records, so a single record the database refuses is
    /// isolated instead of failing every step of the file forever. At a step
    /// of one record, [`SEAL_REPLAY_POISON_FAILURES`] failures in a row at the
    /// same position skip that record.
    fn on_replay_flush_failed(
        &mut self,
        path: &Path,
        offset: u64,
        flush_err: &anyhow::Error,
        outcome: &mut ReplayOutcome,
    ) {
        let Some(cursor) = self.cursor.as_mut() else {
            return;
        };
        if cursor.step_records > 1 {
            cursor.step_records /= 2;
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?flush_err,
                ?path,
                offset,
                next_step_records = cursor.step_records,
                "seal replay: flush failed — the file keeps its place, the next step reads \
                 half as many records, and the replay waits for the database to be healthy \
                 again"
            );
            return;
        }
        cursor.single_record_failures = cursor.single_record_failures.saturating_add(1);
        if cursor.single_record_failures < SEAL_REPLAY_POISON_FAILURES {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?flush_err,
                ?path,
                offset,
                "seal replay: flush of a single spilled seal failed — retrying it once more \
                 after the database is healthy again"
            );
            return;
        }
        let record_bytes = u64::try_from(SEAL_SPILL_RECORD_SIZE).unwrap_or(u64::MAX);
        cursor.offset = cursor.offset.saturating_add(record_bytes);
        cursor.single_record_failures = 0;
        outcome.records_skipped += 1;
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            ?flush_err,
            ?path,
            offset,
            "seal replay: the database refused this one spilled seal in every attempt — \
             skipped; its bytes stay in the file, which moves to archive/"
        );
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
        while records_read < step_records {
            match reader.read_exact(&mut buf) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
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
            // The same format-version gate as the boot drain.
            if buf[7] != SEAL_SPILL_FORMAT_VERSION {
                outcome.records_skipped += 1;
                continue;
            }
            match SerializedSeal::from_bytes(&buf).and_then(|s| s.try_into_buffered_seal()) {
                Some(seal) => self.batch.push(seal),
                None => outcome.records_skipped += 1,
            }
        }
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
        for path in &live {
            let Some(name) = path.file_name() else {
                continue;
            };
            let target = free_path(&replaying, name);
            match std::fs::rename(path, &target) {
                Ok(()) => moved += 1,
                Err(err) => warn!(?path, ?err, "seal replay: cannot stage a spill file"),
            }
        }
        moved
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
        let mut older = current;
        older[7] = SEAL_SPILL_FORMAT_VERSION - 1;
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
        stale[7] = SEAL_SPILL_FORMAT_VERSION.wrapping_sub(1);
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
}
