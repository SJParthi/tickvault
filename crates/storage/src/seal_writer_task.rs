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
use crate::seal_spill::{SEAL_SPILL_RECORD_SIZE, SerializedSeal};
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
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?flush_err,
                count = popped.len(),
                "candle flush failed — rescuing in-flight seals to spill/DLQ"
            );
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
// 1. **Stage.** Every `seals-*` file in the spill / DLQ dir is MOVED into
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

/// Filename prefix shared by both the spill (`.bin`) and DLQ (`.ndjson`)
/// daily files (`seals-YYYY-MM-DD.*`).
const SEAL_FILE_PREFIX: &str = "seals-";

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

/// Moves every `seals-*` file in `dir` into `dir/replaying/` and returns the
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

/// `true` for a regular file named `seals-*.bin` or `seals-*.ndjson`.
fn is_seal_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with(SEAL_FILE_PREFIX) && staged_kind(path).is_some()
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
        // Same format-version gate as `SealSpillWriter::read_all`: a byte-7 of
        // 0 is a pre-renumber record whose tf ordinal lives in the OLD
        // 12-frame space and would silently mis-decode.
        if buf[7] == 0 {
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
        for chunk in records.chunks(batch) {
            let mut appended = 0usize;
            for record in chunk {
                let Some(seal) = record.try_into_buffered_seal() else {
                    // Forward-compat guard: unknown tf ordinal.
                    outcome.records_undecodable += 1;
                    outcome.seals_recovered = outcome.seals_recovered.saturating_sub(1);
                    continue;
                };
                if let Err(append_err) = writer.append_seal(&seal) {
                    error!(
                        code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                        ?append_err,
                        security_id = seal.security_id,
                        "seal recovery: append failed for a recovered seal"
                    );
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

        if file_ok {
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

    outcome
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
        state.oi_pct_from_prev_day = -0.2;
        state.volume_pct_from_prev_day = 12.3;
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
            mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0),
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
                    1_716_000_900 + i as u32,
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
                    1_716_000_900 + i as u32,
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
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0), now);
        pipeline.submit(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0), now);
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
        pipeline.submit(mk_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0), now);
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
                    1_716_000_900 + i as u32,
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
                    1_716_000_900 + i as u32,
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
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0), now);
        pipeline.submit(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0), now);
        let mut writer = ShadowCandleWriter::for_test();
        drain_once(&mut pipeline, &mut writer, 16, now);
        assert_eq!(
            pipeline.ring_len(),
            0,
            "rescued seals MUST go to spill, NOT re-buffer into ring"
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
            pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900 + i, 100.0), now);
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
            pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900 + i, 100.0), now);
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
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0), now);
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
        pipeline.submit(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0), now);
        pipeline.submit(mk_seal(13, 1, TfIndex::M1, 1_716_001_500, 200.0), now);
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
}
