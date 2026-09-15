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

use std::io::{BufRead, BufReader, Seek};
use std::path::{Path, PathBuf};

use tracing::{error, info, warn};

use tickvault_common::error_code::ErrorCode;
use tickvault_trading::candles::BufferedSeal;

use crate::seal_absorption::{SealAbsorptionPipeline, SubmitOutcome};
use crate::seal_dlq::SealDlqRecord;
use crate::seal_recovery_guard::{MAX_RECOVERY_BATCH, RecoveredBatch, RecoveryRefusal};
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
// 2. **Reconcile.** Completely decoded originals enter bounded batches.
//    Applied database rows are read before writes: missing keys may insert,
//    exact full-row duplicates are skipped, and differing rows are refused.
// 3. **Confirm.** ONLY after WAL application and full-row readback does a file move
//    `replaying/` → `archive/`. `archive/` is never re-globbed, so confirmed
//    history never re-injects.
// 4. **Fail-closed.** A failed flush leaves the file in `replaying/` and
//    STOPS the drain (QuestDB is down; the next file would just burn another
//    bounded flush timeout). Next boot re-globs it.
//
// ## Recovery does not invent cross-process revision ordering
//
// - **File side:** no-replace moves publish a hard link before removing the
//   source. An interrupted move can leave both names, with no original bytes
//   replaced. An extra name can be replayed again; DEDUP prevents duplicate
//   keys, but cannot prevent an older revision replacing a newer row.
//   Recovered seals are deliberately NOT re-submitted into
//   the absorption pipeline: doing so would let a still-dead QuestDB rescue
//   them into a FRESH spill file while the staged copy also survives,
//   multiplying the on-disk set on every failed boot.
// - **DB side:** candle DEDUP UPSERT KEYS `(ts, security_id, segment, feed)`
//   use last-write-wins, so recovery never intentionally overwrites a present
//   key. bucket_revision restarts with the aggregator and cannot order boots.
//   The boot producer barrier excludes the managed live writer; independent
//   concurrent writers are unsupported because SQL reads + ILP are not an
//   atomic compare-and-insert. Readback proves applied equality, not a
//   hardware/power-loss guarantee. Archived originals remain retained.
//
// ## Honesty (Rule 11 — no false-OK)
//
// `seals_left_pending` is reported to the caller and surfaced as a counter +
// an `error!`. Seals sitting in `replaying/` are not confirmed in QuestDB;
// some may already be present after a partial ACK. Retention proves neither
// absence nor the winning revision in the database.

/// Staging directory for files that have been read back but whose re-ingest
/// is not yet confirmed. Re-globbed on every boot until confirmed.
pub const SEAL_REPLAYING_SUBDIR: &str = "replaying";

/// Confirmed-history directory. NEVER re-globbed, so an archived file can
/// never re-inject.
pub const SEAL_ARCHIVE_SUBDIR: &str = "archive";

/// Filename prefix shared by legacy and versioned spill/DLQ records
/// daily files (`seals-YYYY-MM-DD.*`).
const SEAL_FILE_PREFIX: &str = "seals-";

/// Outcome of one [`drain_recovered_seals`] pass. Every field maps to a
/// `tv_seal_writer_drain_total{kind=...}` label emitted by the writer loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootDrainOutcome {
    /// Files moved into (or already sitting in) `replaying/` this pass.
    pub files_staged: usize,
    /// Active-timeframe records successfully decoded off disk.
    pub seals_recovered: usize,
    /// Recovered records confirmed equal to applied QuestDB rows (new inserts
    /// plus already identical rows). HTTP acceptance alone does not count.
    pub seals_reingested: usize,
    /// Confirmed records skipped because the persisted payload matched,
    /// including recognized 18-column legacy duplicates whose added fields
    /// remain NULL. Included in seals_reingested; no new ILP row was sent.
    pub seals_already_present: usize,
    /// Files confirmed and moved to `archive/`.
    pub files_archived: usize,
    /// Known files still in `replaying/` (flush failed or never attempted).
    pub files_left_pending: usize,
    /// Known decoded seals not confirmed in QuestDB. A failed stream leaves
    /// an unread tail, whose additional record count is explicitly unknown.
    pub seals_left_pending: usize,
    /// Undecodable records or damaged boundaries. Their originals remain
    /// pending; a damaged suffix can contain an unknown number of records.
    pub records_undecodable: usize,
    /// Decoded records belonging to intentionally retired timeframes. They
    /// are known pending history, not corruption; their originals are retained.
    pub records_retired_timeframe: usize,
    /// Whole originals deferred before any sink append because they contain
    /// retired history. Active siblings are pending too, preventing an old
    /// revision from overwriting a later correction on a subsequent boot.
    pub files_deferred_retired_timeframe: usize,
    /// Whole originals deferred before any sink append because their records,
    /// framing, complete read or rewind could not be verified. These originals
    /// must be repaired/reconciled before replaying even their valid siblings.
    pub files_deferred_invalid: usize,
    /// Whole originals retained after a valid key matched differing stored
    /// values. Revision magnitude is never used to override a conflict.
    pub files_deferred_conflict: usize,
    /// Recovery was invoked without the managed-writer boot exclusion.
    pub files_deferred_ordering_unverified: usize,
    /// The runner was already recovered or started live processing, so no
    /// staging inspection was allowed under a false boot-exclusion claim.
    pub ordering_exclusion_unverified: bool,
    /// Directory/entry inspection or staging failures. Zero known files does
    /// not mean an empty backlog when discovery was incomplete.
    pub discovery_errors: usize,
    /// At least one retained file/directory was not completely decoded or
    /// inspected. The numeric pending counters are only lower bounds then.
    pub pending_count_unknown: bool,
}

impl BootDrainOutcome {
    /// `true` if nothing was found on disk (the healthy fresh-boot path).
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.files_staged == 0
            && self.discovery_errors == 0
            && !self.pending_count_unknown
            && !self.ordering_exclusion_unverified
    }
}

/// Which on-disk format a staged file carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StagedKind {
    /// Versioned 128/176-byte binary records (`SealSpillWriter`).
    Spill,
    /// NDJSON lines (`SealDlqWriter`).
    Dlq,
}

/// Moves every `seals-*` file in `dir` into `dir/replaying/` and returns the
/// full staged set (including leftovers from a prior crashed boot).
///
/// Discovery and staging failures are returned, never flattened into an empty
/// backlog. All directory entries are inspected before moving any live file.
fn stage_pending_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let live = list_seal_files(dir, true)?;
    if live.is_empty() && !dir.try_exists()? {
        return Ok(Vec::new());
    }
    let replaying = dir.join(SEAL_REPLAYING_SUBDIR);
    std::fs::create_dir_all(&replaying)?;
    // Refuse an unreadable pre-existing staging directory before moving any
    // live originals. The final listing also detects later inspection errors.
    let _ = list_seal_files(&replaying, false)?;
    for path in live {
        crate::seal_spill::move_seal_file_no_replace(&path, &replaying)?;
    }
    list_seal_files(&replaying, false)
}

fn list_seal_files(dir: &Path, absent_is_empty: bool) -> std::io::Result<Vec<PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => collect_seal_files(entries),
        Err(error) if absent_is_empty && error.kind() == std::io::ErrorKind::NotFound => {
            // A dangling link is an inspection failure, not an absent fresh
            // directory. Only an actual absent pathname is a clean no-op.
            match std::fs::symlink_metadata(dir) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn collect_seal_files(
    entries: impl IntoIterator<Item = std::io::Result<std::fs::DirEntry>>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        // Propagate per-entry metadata failures even when another entry was
        // readable. Never return an incomplete list as a complete discovery.
        let file_type = entry.file_type()?;
        if is_seal_name(&path) {
            if !file_type.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "seal recovery name does not identify a regular file",
                ));
            }
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Recognizes legacy, v3 and v4 spill/DLQ names, including repeated collision
/// suffixes. Filesystem inspection belongs to the fallible discovery function.
fn is_seal_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with(SEAL_FILE_PREFIX) && staged_kind(path).is_some()
}

/// Classifies a staged file by extension. Collision-suffixed names
/// (`seals-2026-08-11.bin.1`) keep their kind via the embedded extension.
fn staged_kind(path: &Path) -> Option<StagedKind> {
    let name = path.file_name().and_then(|n| n.to_str())?;
    // A file can collide at staging and again at archive. Every numeric
    // suffix must remain discoverable. Recognize the historical broken
    // `.overflow` fallback too, but the no-replace mover never creates it.
    let mut base = name;
    while let Some((head, tail)) = base.rsplit_once('.') {
        if tail != "overflow" && (tail.is_empty() || !tail.bytes().all(|c| c.is_ascii_digit())) {
            break;
        }
        base = head;
    }
    match base.rsplit_once('.').map(|(_, extension)| extension) {
        Some(
            "bin"
            | crate::seal_spill::SEAL_SPILL_V3_EXTENSION
            | crate::seal_spill::SEAL_SPILL_V4_EXTENSION,
        ) => Some(StagedKind::Spill),
        Some(
            "ndjson"
            | crate::seal_dlq::SEAL_DLQ_V3_EXTENSION
            | crate::seal_dlq::SEAL_DLQ_V4_EXTENSION,
        ) => Some(StagedKind::Dlq),
        _ => None,
    }
}

/// Recovery accepts the current fixed-field DLQ JSON within a finite line
/// budget. Current writers emit numeric fields and a short known feed name;
/// oversized/corrupt/future records remain in the original for inspection.
/// The cap bounds memory, not the time needed to skip a damaged long line.
const SEAL_DLQ_RECORD_MAX_BYTES: usize = 16 * 1024;

/// One bounded decoding step. A damaged binary boundary cannot be guessed;
/// a damaged NDJSON line can safely resume after its newline delimiter.
enum StagedRead {
    Record(SerializedSeal),
    Undecodable { terminal: bool },
    Eof,
}

/// Read one DLQ line with fixed storage. Bytes past the limit are consumed
/// without allocation until newline/EOF so a later valid line can recover.
/// `None` means clean EOF before any byte, not an oversized or unreadable row.
fn read_bounded_dlq_line<R: BufRead>(
    reader: &mut R,
    line: &mut [u8; SEAL_DLQ_RECORD_MAX_BYTES],
) -> std::io::Result<Option<(usize, bool)>> {
    let mut used = 0usize;
    let mut oversized = false;
    let mut saw_bytes = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_bytes.then_some((used, oversized)));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let payload_len = newline.unwrap_or(available.len());
        let consumed = payload_len + usize::from(newline.is_some());
        let copied = payload_len.min(line.len() - used);
        line[used..used + copied].copy_from_slice(&available[..copied]);
        used += copied;
        oversized |= copied != payload_len;
        reader.consume(consumed);
        saw_bytes = true;
        if newline.is_some() {
            return Ok(Some((used, oversized)));
        }
    }
}

fn read_staged_record<R: BufRead>(
    reader: &mut R,
    kind: StagedKind,
    binary: &mut [u8; SEAL_SPILL_RECORD_SIZE],
    line: &mut [u8; SEAL_DLQ_RECORD_MAX_BYTES],
) -> std::io::Result<StagedRead> {
    match kind {
        StagedKind::Spill => {
            match crate::seal_spill::read_full_record(reader, binary).map_err(|error| {
                let kind = error
                    .downcast_ref::<std::io::Error>()
                    .map_or(std::io::ErrorKind::InvalidData, std::io::Error::kind);
                std::io::Error::new(kind, error)
            })? {
                Some(size) => Ok(match SerializedSeal::from_bytes(&binary[..size]) {
                    Some(seal) => StagedRead::Record(seal),
                    None => StagedRead::Undecodable { terminal: true },
                }),
                None => Ok(StagedRead::Eof),
            }
        }
        StagedKind::Dlq => loop {
            let Some((used, oversized)) = read_bounded_dlq_line(reader, line)? else {
                return Ok(StagedRead::Eof);
            };
            if oversized {
                return Ok(StagedRead::Undecodable { terminal: false });
            }
            if line[..used].iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            return Ok(
                match serde_json::from_slice::<SealDlqRecord>(&line[..used]) {
                    Ok(record) => StagedRead::Record(SerializedSeal::from(&record)),
                    Err(_) => StagedRead::Undecodable { terminal: false },
                },
            );
        },
    }
}

#[derive(Default)]
struct StagedPreflight {
    records_decoded: usize,
    active_records: usize,
    retired_records: usize,
    records_undecodable: usize,
    reached_eof: bool,
    io_failed: bool,
}

impl StagedPreflight {
    fn pending_unknown(&self) -> bool {
        !self.reached_eof || self.records_undecodable != 0
    }

    fn defers_whole_file(&self) -> bool {
        self.retired_records != 0 || self.io_failed || self.pending_unknown()
    }
}

/// Inspect the entire recognizable original before touching the sink. A
/// retired/unknown record, malformed boundary, missing EOF or read failure
/// keeps the whole file pending, including valid active siblings. Replaying
/// those siblings on every boot could overwrite newer revisions under
/// last-write-wins database DEDUP. Buffers stay independent of file size.
fn preflight_staged_stream<R: BufRead>(
    reader: &mut R,
    kind: StagedKind,
    path: &Path,
) -> StagedPreflight {
    let mut outcome = StagedPreflight::default();
    let mut binary = [0u8; SEAL_SPILL_RECORD_SIZE];
    let mut line = [0u8; SEAL_DLQ_RECORD_MAX_BYTES];
    loop {
        match read_staged_record(reader, kind, &mut binary, &mut line) {
            Ok(StagedRead::Record(record)) => {
                outcome.records_decoded = outcome.records_decoded.saturating_add(1);
                if tickvault_trading::candles::TfIndex::is_retired_storage_ordinal(
                    record.tf_ordinal,
                ) {
                    outcome.retired_records = outcome.retired_records.saturating_add(1);
                } else if record.tf().is_some() {
                    outcome.active_records = outcome.active_records.saturating_add(1);
                } else {
                    outcome.records_undecodable = outcome.records_undecodable.saturating_add(1);
                }
            }
            Ok(StagedRead::Eof) => {
                outcome.reached_eof = true;
                return outcome;
            }
            Ok(StagedRead::Undecodable { terminal }) => {
                outcome.records_undecodable = outcome.records_undecodable.saturating_add(1);
                if terminal {
                    return outcome;
                }
            }
            Err(error) => {
                // Both framing failures and native I/O failures defer the
                // entire original. Keep their reasons separate in diagnostics:
                // a damaged boundary cannot classify the unread suffix.
                outcome.io_failed = !matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
                );
                outcome.records_undecodable = outcome.records_undecodable.saturating_add(1);
                warn!(
                    ?path,
                    ?error,
                    "staged seal preflight incomplete — original remains unconfirmed"
                );
                return outcome;
            }
        }
    }
}

#[derive(Default)]
struct StreamReplayOutcome {
    /// Decoded serialized records, including an unsupported timeframe.
    records_decoded: usize,
    /// Decoded records whose timeframe is supported.
    seals_recovered: usize,
    seals_reingested: usize,
    seals_already_present: usize,
    records_undecodable: usize,
    records_retired_timeframe: usize,
    reached_eof: bool,
    sink_failed: bool,
    conflict: bool,
    ordering_unverified: bool,
}

impl StreamReplayOutcome {
    fn complete(&self) -> bool {
        self.reached_eof
            && !self.sink_failed
            && !self.conflict
            && !self.ordering_unverified
            && self.records_undecodable == 0
            && self.records_retired_timeframe == 0
            && self.records_decoded == self.seals_reingested
    }

    fn pending_unknown(&self) -> bool {
        !self.reached_eof || self.records_undecodable != 0
    }
}

/// Every recovery write passes the separate reconciliation seam. A partial
/// ACK leaves the unchanged original; the next boot reads applied rows and
/// skips exact duplicates before attempting the still-missing keys.
fn flush_recovered_batch<S: SealSink>(
    writer: &mut S,
    path: &Path,
    pending: &mut Vec<BufferedSeal>,
    outcome: &mut StreamReplayOutcome,
) -> bool {
    if pending.is_empty() {
        return true;
    }
    let confirmed = writer.recover_batch(pending);
    match confirmed {
        Ok(batch) if batch.inserted.checked_add(batch.identical) == Some(pending.len()) => {
            outcome.seals_reingested = outcome.seals_reingested.saturating_add(pending.len());
            outcome.seals_already_present = outcome
                .seals_already_present
                .saturating_add(batch.identical);
            pending.clear();
            true
        }
        other => {
            let refusal = match other {
                Err(refusal) => refusal,
                Ok(_) => RecoveryRefusal::Unconfirmed(anyhow::anyhow!(
                    "recovery sink returned an incomplete confirmation"
                )),
            };
            outcome.conflict = matches!(&refusal, RecoveryRefusal::Conflict);
            outcome.ordering_unverified = matches!(&refusal, RecoveryRefusal::OrderingUnverified);
            outcome.sink_failed = !outcome.conflict;
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?refusal,
                ?path,
                pending = pending.len(),
                "seal recovery: batch not confirmed — original retained; no conflicting row is authorized for overwrite"
            );
            writer.discard_pending();
            pending.clear();
            false
        }
    }
}

/// Stream one staged file. At most `max_batch.clamp(1, MAX_RECOVERY_BATCH)`
/// seals enter reconciliation, with fixed reader/line buffers.
/// No allocation grows with file length. Runtime work still scales with bytes,
/// and neither this loop nor the boot barrier bounds blocking kernel I/O.
fn replay_staged_stream<S: SealSink, R: BufRead>(
    writer: &mut S,
    reader: &mut R,
    kind: StagedKind,
    max_batch: usize,
    path: &Path,
) -> StreamReplayOutcome {
    let mut outcome = StreamReplayOutcome::default();
    let batch = max_batch.clamp(1, MAX_RECOVERY_BATCH);
    let mut pending = Vec::with_capacity(batch);
    let mut binary = [0u8; SEAL_SPILL_RECORD_SIZE];
    let mut line = [0u8; SEAL_DLQ_RECORD_MAX_BYTES];
    loop {
        let record = match read_staged_record(reader, kind, &mut binary, &mut line) {
            Ok(StagedRead::Record(record)) => record,
            Ok(StagedRead::Eof) => {
                outcome.reached_eof = true;
                break;
            }
            Ok(StagedRead::Undecodable { .. }) => {
                outcome.records_undecodable = outcome.records_undecodable.saturating_add(1);
                break;
            }
            Err(error) => {
                outcome.records_undecodable = outcome.records_undecodable.saturating_add(1);
                warn!(
                    ?path,
                    ?error,
                    "staged seal stream incomplete — original retained"
                );
                break;
            }
        };
        outcome.records_decoded = outcome.records_decoded.saturating_add(1);
        if tickvault_trading::candles::TfIndex::is_retired_storage_ordinal(record.tf_ordinal) {
            outcome.records_retired_timeframe = outcome.records_retired_timeframe.saturating_add(1);
            break;
        }
        let Some(seal) = record.try_into_buffered_seal() else {
            outcome.records_undecodable = outcome.records_undecodable.saturating_add(1);
            break;
        };
        outcome.seals_recovered = outcome.seals_recovered.saturating_add(1);
        pending.push(seal);
        if pending.len() == batch
            && !flush_recovered_batch(writer, path, &mut pending, &mut outcome)
        {
            return outcome;
        }
    }
    // A failure during the second read must not flush its current prefix.
    // Earlier confirmed batches cannot be undone here. An unchanged retry
    // skips identical applied rows and refuses conflicts; original bytes stay intact.
    if !outcome.reached_eof
        || outcome.records_undecodable != 0
        || outcome.records_retired_timeframe != 0
    {
        if !pending.is_empty() {
            writer.discard_pending();
        }
        return outcome;
    }
    flush_recovered_batch(writer, path, &mut pending, &mut outcome);
    outcome
}

/// Moves a confirmed file from `replaying/` to `archive/`.
fn archive_staged(path: &Path) -> std::io::Result<()> {
    let root = path.parent().and_then(Path::parent).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "seal archive path has no staging root",
        )
    })?;
    let archive = root.join(SEAL_ARCHIVE_SUBDIR);
    std::fs::create_dir_all(&archive)?;
    crate::seal_spill::move_seal_file_no_replace(path, &archive).map(|_| ())
}

/// The narrow slice of [`ShadowCandleWriter`] the recovery drain needs.
///
/// Exists so the recovery path is testable end-to-end WITHOUT a live
/// QuestDB: `ShadowCandleWriter::for_test()` is permanently disconnected, so
/// its `flush()` can only ever fail, which would leave the success path — the
/// half that actually proves seals are re-ingested and files archived —
/// untestable and therefore unproven.
///
/// Cold path, static dispatch (`impl Trait`), with bounded batch allocation.
pub trait SealSink {
    /// Append one seal to the wire buffer.
    fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()>;
    /// Flush the wire buffer to the database.
    fn flush(&mut self) -> anyhow::Result<()>;
    /// Discard the local retained buffer after a failed append/flush (poison recovery).
    fn discard_pending(&mut self);
    /// Boot-recovery-only admission, separate from live last-write-wins
    /// append. Implementations must confirm applied full-row equality or the
    /// explicit unchanged18-column legacy contract, refusing other differences.
    /// An unimplemented guard is a retained backlog.
    fn recover_batch(
        &mut self,
        _seals: &[BufferedSeal],
    ) -> Result<RecoveredBatch, RecoveryRefusal> {
        Err(RecoveryRefusal::OrderingUnverified)
    }
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
    fn recover_batch(&mut self, seals: &[BufferedSeal]) -> Result<RecoveredBatch, RecoveryRefusal> {
        Self::reconcile_recovered_batch(self, seals)
    }
}

/// Transport-only fixture adapter. These sinks own an isolated in-memory
/// destination; production reconciliation is tested separately against a
/// conflicting-row/WAL/partial-ACK store in seal_recovery_guard.
#[cfg(test)]
pub(crate) fn recover_fixture_batch<S: SealSink + ?Sized>(
    sink: &mut S,
    seals: &[BufferedSeal],
) -> Result<RecoveredBatch, RecoveryRefusal> {
    for seal in seals {
        sink.append_seal(seal)?;
    }
    sink.flush()?;
    Ok(RecoveredBatch {
        inserted: seals.len(),
        identical: 0,
    })
}

/// Boot-time recovery: reads every orphaned spill / DLQ file back and
/// re-ingests it into QuestDB through `writer`.
///
/// Cold path — runs ONCE at writer-loop startup, before the drain ticker.
/// Allowed to allocate.
///
/// Stops at the first append or flush failure (the remaining files
/// stay staged for the next boot). Reports known pending counts and explicitly
/// marks an unknown total when inspection, a damaged suffix, or an unread tail
/// prevents counting. Record memory is independent of file length; the sink
/// buffer contains at most `max_batch.clamp(1, MAX_RECOVERY_BATCH)` seals.
/// Every file is preflighted with fixed buffers before any sink append. A
/// retired/unknown ID, damaged record, incomplete read or I/O failure defers
/// its whole original without writing even a valid prefix. An active-only,
/// completely decoded file is
/// then rewound on the same open handle and replayed in bounded batches, so
/// successful recovery normally reads that file twice. Boot ownership excludes
/// external file mutation between those passes. This framing gate does not
/// infer a revision order. The separate recovery sink admits absent keys and
/// exact full-row duplicates only, with WAL+readback confirmation before
/// archive. A conflict retains the original for reconciliation. This contract
/// requires the one-time managed-writer boot exclusion; external concurrent
/// writers are not supported by SQL-read/ILP-write reconciliation.
pub fn drain_recovered_seals<S: SealSink>(
    writer: &mut S,
    spill_dir: &Path,
    dlq_dir: &Path,
    max_batch: usize,
) -> BootDrainOutcome {
    let mut outcome = BootDrainOutcome::default();
    let batch = max_batch.max(1);

    let mut staged = Vec::new();
    for directory in [Some(spill_dir), (dlq_dir != spill_dir).then_some(dlq_dir)]
        .into_iter()
        .flatten()
    {
        match stage_pending_files(directory) {
            Ok(files) => staged.extend(files),
            Err(error) => {
                outcome.discovery_errors += 1;
                outcome.pending_count_unknown = true;
                error!(
                    code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                    ?directory,
                    ?error,
                    "seal recovery discovery INCOMPLETE — pending count is unknown; originals retained"
                );
            }
        }
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
            // The sink refused a batch — count known untouched files without
            // reading their records or retrying the same failed sink per file.
            outcome.files_left_pending += 1;
            outcome.pending_count_unknown = true;
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
        let opened = staged_kind(path)
            .and_then(|kind| std::fs::File::open(path).ok().map(|file| (kind, file)));
        let Some((kind, file)) = opened else {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                ?path,
                "seal recovery: staged file could not be read or classified — original remains pending"
            );
            outcome.files_left_pending += 1;
            outcome.pending_count_unknown = true;
            continue;
        };
        let mut reader = BufReader::new(file);
        let preflight = preflight_staged_stream(&mut reader, kind, path);
        let defer_retired = preflight.retired_records != 0;
        let defer_preflight = preflight.defers_whole_file();
        let rewind = if defer_preflight {
            Ok(())
        } else {
            reader.rewind()
        };
        if defer_preflight || rewind.is_err() {
            outcome.files_left_pending += 1;
            outcome.seals_recovered = outcome
                .seals_recovered
                .saturating_add(preflight.active_records);
            outcome.seals_left_pending = outcome
                .seals_left_pending
                .saturating_add(preflight.records_decoded);
            outcome.records_undecodable = outcome
                .records_undecodable
                .saturating_add(preflight.records_undecodable);
            outcome.records_retired_timeframe = outcome
                .records_retired_timeframe
                .saturating_add(preflight.retired_records);
            outcome.pending_count_unknown |= preflight.pending_unknown();
            if defer_retired {
                outcome.files_deferred_retired_timeframe += 1;
                warn!(
                    ?path,
                    retired_records = preflight.retired_records,
                    active_records_deferred = preflight.active_records,
                    "retired timeframe history: whole original deferred before replay to preserve later database corrections"
                );
            } else {
                outcome.files_deferred_invalid += 1;
                outcome.pending_count_unknown = true;
                let reason = if preflight.io_failed {
                    "io_failure"
                } else if !preflight.reached_eof {
                    "incomplete_stream"
                } else if preflight.records_undecodable != 0 {
                    "undecodable_record_or_timeframe"
                } else {
                    "rewind_failure"
                };
                warn!(
                    ?path,
                    reason,
                    active_records_deferred = preflight.active_records,
                    records_undecodable = preflight.records_undecodable,
                    error = ?rewind.err(),
                    "seal recovery: whole original deferred before sink writes — valid siblings remain pending with unverified history"
                );
            }
            continue;
        }
        let replay = replay_staged_stream(writer, &mut reader, kind, batch, path);
        drop(reader);
        outcome.records_undecodable = outcome
            .records_undecodable
            .saturating_add(replay.records_undecodable);
        outcome.records_retired_timeframe = outcome
            .records_retired_timeframe
            .saturating_add(replay.records_retired_timeframe);
        outcome.seals_recovered = outcome
            .seals_recovered
            .saturating_add(replay.seals_recovered);
        outcome.seals_reingested = outcome
            .seals_reingested
            .saturating_add(replay.seals_reingested);
        outcome.seals_already_present = outcome
            .seals_already_present
            .saturating_add(replay.seals_already_present);
        outcome.files_deferred_conflict += usize::from(replay.conflict);
        outcome.files_deferred_ordering_unverified += usize::from(replay.ordering_unverified);
        outcome.pending_count_unknown |= replay.pending_unknown();
        halted = replay.sink_failed;
        if replay.complete() {
            match archive_staged(path) {
                Ok(()) => outcome.files_archived += 1,
                Err(err) => {
                    // A failed no-replace move/barrier retains the bytes at
                    // source, destination or both. A remaining staged name
                    // can replay again; the guard rechecks full-row equality
                    // and skips identical data rather than blindly resending.
                    warn!(?path, ?err, "seal recovery: archive move not confirmed");
                    outcome.pending_count_unknown = true;
                    if std::fs::symlink_metadata(path).is_ok() {
                        outcome.files_left_pending += 1;
                    }
                }
            }
        } else {
            outcome.files_left_pending += 1;
            outcome.seals_left_pending = outcome.seals_left_pending.saturating_add(
                replay
                    .records_decoded
                    .saturating_sub(replay.seals_reingested),
            );
            // Corruption does not strand other healthy files. A sink failure
            // stops this pass without decoding unbounded unread tails merely
            // to supply a precise count that is already marked unknown.
        }
    }

    if outcome.files_left_pending > 0 || outcome.pending_count_unknown {
        error!(
            code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
            files_pending = outcome.files_left_pending,
            seals_pending = outcome.seals_left_pending,
            seals_reingested = outcome.seals_reingested,
            discovery_errors = outcome.discovery_errors,
            pending_count_unknown = outcome.pending_count_unknown,
            records_retired_timeframe = outcome.records_retired_timeframe,
            files_deferred_retired_timeframe = outcome.files_deferred_retired_timeframe,
            files_deferred_invalid = outcome.files_deferred_invalid,
            files_deferred_conflict = outcome.files_deferred_conflict,
            files_deferred_ordering_unverified = outcome.files_deferred_ordering_unverified,
            "seal recovery INCOMPLETE — retained files or unconfirmed discovery require inspection"
        );
    } else {
        info!(
            files_archived = outcome.files_archived,
            seals_reingested = outcome.seals_reingested,
            records_undecodable = outcome.records_undecodable,
            seals_already_present = outcome.seals_already_present,
            "seal recovery complete — every recovered record confirmed against applied rows"
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

    // Small-fixture collection only; production replays the same decoder one
    // record at a time and never constructs a whole-file record vector.
    fn collect_staged_fixture(
        path: &Path,
        kind: StagedKind,
    ) -> Option<(Vec<SerializedSeal>, usize)> {
        let mut reader = BufReader::new(std::fs::File::open(path).ok()?);
        let mut binary = [0u8; SEAL_SPILL_RECORD_SIZE];
        let mut line = [0u8; SEAL_DLQ_RECORD_MAX_BYTES];
        let mut records = Vec::new();
        let mut undecodable = 0;
        loop {
            match read_staged_record(&mut reader, kind, &mut binary, &mut line) {
                Ok(StagedRead::Record(record)) => records.push(record),
                Ok(StagedRead::Eof) => return Some((records, undecodable)),
                Ok(StagedRead::Undecodable { terminal }) => {
                    undecodable += 1;
                    if terminal {
                        return Some((records, undecodable));
                    }
                }
                Err(_) => return Some((records, undecodable + 1)),
            }
        }
    }

    fn read_staged_spill(path: &Path) -> Option<(Vec<SerializedSeal>, usize)> {
        collect_staged_fixture(path, StagedKind::Spill)
    }

    fn read_staged_dlq(path: &Path) -> Option<(Vec<SerializedSeal>, usize)> {
        collect_staged_fixture(path, StagedKind::Dlq)
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
            prod.contains("outcome.files_left_pending += 1;\n            outcome.pending_count_unknown = true;\n            continue;"),
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
        state.net_volume_signed = -1234;
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
    #[test]
    fn staged_reader_salvages_complete_prefix_and_marks_partial_original_pending() {
        let (spill, dlq) = temp_pair("v3-staged-reader");
        std::fs::create_dir_all(&spill).expect("mkdir");
        let record = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0));
        let current = record.to_bytes();
        let mut old = current[..crate::seal_spill::SEAL_SPILL_LEGACY_RECORD_SIZE].to_vec();
        old[7] = 2;
        let mut bytes = old;
        bytes.extend_from_slice(&current);
        let path = spill.join("seals-2026-01-01.bin");
        std::fs::write(&path, &bytes).expect("mixed file");
        let (rows, rejected) = read_staged_spill(&path).expect("complete mixed file");
        assert_eq!(rows.len(), 2);
        assert_eq!(rejected, 0);
        assert_eq!(
            rows[0].metadata,
            tickvault_trading::candles::CandleMetadata::UNKNOWN
        );
        assert_eq!(rows[1], record);
        bytes.extend_from_slice(&current[..20]);
        std::fs::write(&path, &bytes).expect("interrupted final record");
        let (prefix, rejected) = read_staged_spill(&path).expect("recoverable complete prefix");
        assert_eq!(prefix.len(), 2);
        assert_eq!(rejected, 1, "the original cannot be confirmed as complete");
        assert_eq!(std::fs::read(&path).expect("retained"), bytes);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn one_rejected_append_cannot_archive_a_partially_recovered_file() {
        #[derive(Default)]
        struct TestSink {
            reject_id: Option<u64>,
            accepted: usize,
        }
        impl SealSink for TestSink {
            fn recover_batch(
                &mut self,
                seals: &[BufferedSeal],
            ) -> Result<
                crate::seal_recovery_guard::RecoveredBatch,
                crate::seal_recovery_guard::RecoveryRefusal,
            > {
                crate::seal_writer_task::recover_fixture_batch(self, seals)
            }

            fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
                if self.reject_id == Some(seal.security_id) {
                    anyhow::bail!("injected append refusal");
                }
                self.accepted += 1;
                Ok(())
            }
            fn flush(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn discard_pending(&mut self) {}
        }
        let (spill, dlq) = temp_pair("retain-rejected-append");
        std::fs::create_dir_all(&spill).expect("mkdir");
        let mut bytes = Vec::new();
        for id in [13, 25] {
            bytes.extend_from_slice(
                &SerializedSeal::from(&mk_seal(id, 0, TfIndex::M1, 1_716_023_700, 100.0))
                    .to_bytes(),
            );
        }
        std::fs::write(spill.join("seals-2026-01-01.bin"), bytes).expect("write");
        let mut refusal = TestSink {
            reject_id: Some(25),
            accepted: 0,
        };
        let first = drain_recovered_seals(&mut refusal, &spill, &dlq, 64);
        assert_eq!(first.files_archived, 0);
        assert_eq!(first.files_left_pending, 1);
        assert_eq!(
            first.seals_reingested, 0,
            "a failed append discards the unflushed batch"
        );
        assert_eq!(first.seals_left_pending, 2);
        assert!(
            !first.pending_count_unknown,
            "the final batch is admitted after a complete EOF read"
        );
        let mut healthy = TestSink::default();
        let second = drain_recovered_seals(&mut healthy, &spill, &dlq, 64);
        assert_eq!(second.seals_reingested, 2);
        assert_eq!(second.files_archived, 1);
        assert_eq!(second.files_left_pending, 0);
        cleanup(&spill, &dlq);
    }

    #[test]
    fn v4_write_names_are_invisible_to_rollback_discovery_but_new_recovery_finds_them() {
        // Reference discovery contract read from 7f97c32 and ede0a795:
        // a regular seals-* file whose collision-stripped name ends with
        // .bin or .ndjson. In particular .cseal3 and .cseal3json never match.
        fn rollback_recognizes(path: &Path) -> bool {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let base = name.rsplit_once('.').map_or(name, |(head, tail)| {
                if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                    head
                } else {
                    name
                }
            });
            path.is_file()
                && name.starts_with("seals-")
                && (base.ends_with(".bin") || base.ends_with(".ndjson"))
        }
        let (spill, dlq) = temp_pair("rollback-namespace-v3");
        let now = jan1_noon_utc();
        let record = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0));
        let binary = crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        binary.append_seal(&record, now).expect("new spill");
        let json = crate::seal_dlq::SealDlqWriter::with_dlq_dir_for_test(dlq.clone());
        json.append_record(&SealDlqRecord::from(&record), now)
            .expect("new DLQ");
        let binary_path = binary.spill_path(now);
        let json_path = json.dlq_path(now);
        assert!(!rollback_recognizes(&binary_path));
        assert!(!rollback_recognizes(&json_path));
        assert_eq!(binary_path.parent(), Some(spill.as_path()));
        assert_eq!(json_path.parent(), Some(dlq.as_path()));
        assert_eq!(
            crate::seal_dlq::dlq_bytes_at(&dlq),
            std::fs::metadata(&json_path).expect("size").len()
        );

        let mut legacy =
            record.to_bytes()[..crate::seal_spill::SEAL_SPILL_LEGACY_RECORD_SIZE].to_vec();
        legacy[7] = 2;
        let legacy_path = spill.join("seals-2025-12-31.bin");
        std::fs::write(&legacy_path, legacy).expect("legacy spill");
        assert!(rollback_recognizes(&legacy_path));
        let old_json = dlq.join("seals-2025-12-31.ndjson");
        std::fs::write(&old_json, "{}\n").expect("legacy JSON");
        assert!(rollback_recognizes(&old_json));

        // A restarted new binary discovers old and new records, including a
        // filename collision suffix in the same replaying namespace.
        let staged_spill = stage_pending_files(&spill).expect("stage spill");
        let staged_dlq = stage_pending_files(&dlq).expect("stage DLQ");
        assert_eq!(staged_spill.len(), 2);
        assert_eq!(staged_dlq.len(), 2);
        for path in staged_spill.iter().chain(&staged_dlq) {
            if path.extension().and_then(|e| e.to_str()) == Some("cseal4")
                || path.extension().and_then(|e| e.to_str()) == Some("cseal4json")
            {
                assert!(!rollback_recognizes(path));
                let collision = path.with_file_name(format!(
                    "{}.1",
                    path.file_name().expect("name").to_string_lossy()
                ));
                std::fs::copy(path, &collision).expect("collision fixture");
                assert!(!rollback_recognizes(&collision));
                assert!(is_seal_name(&collision));
            }
        }
        assert!(
            staged_spill
                .iter()
                .all(|path| read_staged_spill(path).is_some())
        );
        assert!(
            staged_dlq
                .iter()
                .all(|path| read_staged_dlq(path).is_some())
        );
        cleanup(&spill, &dlq);
    }

    #[derive(Default)]
    struct RecordingSink {
        accepted: Vec<u64>,
    }

    impl SealSink for RecordingSink {
        fn recover_batch(
            &mut self,
            seals: &[BufferedSeal],
        ) -> Result<
            crate::seal_recovery_guard::RecoveredBatch,
            crate::seal_recovery_guard::RecoveryRefusal,
        > {
            crate::seal_writer_task::recover_fixture_batch(self, seals)
        }

        fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
            self.accepted.push(seal.security_id);
            Ok(())
        }

        fn flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn discard_pending(&mut self) {}
    }

    #[derive(Default)]
    struct StreamingSink {
        pending: usize,
        max_pending: usize,
        pending_id_sum: u64,
        committed: usize,
        committed_id_sum: u64,
        appended: usize,
        flushes: usize,
        discards: usize,
        fail_flush: Option<usize>,
        fail_append: Option<usize>,
        read_position: Option<std::rc::Rc<std::cell::Cell<usize>>>,
        first_flush_position: Option<usize>,
    }

    impl SealSink for StreamingSink {
        fn recover_batch(
            &mut self,
            seals: &[BufferedSeal],
        ) -> Result<
            crate::seal_recovery_guard::RecoveredBatch,
            crate::seal_recovery_guard::RecoveryRefusal,
        > {
            crate::seal_writer_task::recover_fixture_batch(self, seals)
        }

        fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
            self.appended += 1;
            self.pending += 1;
            self.max_pending = self.max_pending.max(self.pending);
            self.pending_id_sum += seal.security_id;
            // Refuse after a local mutation, so the test requires discarding
            // an incomplete batch rather than assuming Err made no changes.
            anyhow::ensure!(
                self.fail_append != Some(self.appended),
                "injected append refusal"
            );
            Ok(())
        }

        fn flush(&mut self) -> anyhow::Result<()> {
            self.flushes += 1;
            if self.first_flush_position.is_none() {
                self.first_flush_position =
                    self.read_position.as_ref().map(|position| position.get());
            }
            anyhow::ensure!(
                self.fail_flush != Some(self.flushes),
                "injected flush refusal"
            );
            self.committed += self.pending;
            self.committed_id_sum += self.pending_id_sum;
            self.pending = 0;
            self.pending_id_sum = 0;
            Ok(())
        }

        fn discard_pending(&mut self) {
            self.discards += 1;
            self.pending = 0;
            self.pending_id_sum = 0;
        }
    }

    struct ObservedRead<R> {
        input: R,
        position: std::rc::Rc<std::cell::Cell<usize>>,
        fail_at: Option<usize>,
    }

    impl<R: std::io::Read> std::io::Read for ObservedRead<R> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let position = self.position.get();
            let remaining = match self.fail_at {
                Some(boundary) if position >= boundary => {
                    return Err(std::io::Error::other("injected non-advancing read error"));
                }
                Some(boundary) => boundary - position,
                None => output.len(),
            };
            let limit = output.len().min(remaining);
            let read = self.input.read(&mut output[..limit])?;
            self.position.set(position + read);
            Ok(read)
        }
    }

    fn serialized_fixture(id: u64) -> SerializedSeal {
        SerializedSeal::from(&mk_seal(id, 0, TfIndex::M1, 1_716_023_700, 100.0))
    }

    #[test]
    fn valid_conflicting_originals_are_retained_while_separate_missing_rows_can_recover() {
        #[derive(Default)]
        struct ConflictSink {
            appended: Vec<u64>,
        }
        impl SealSink for ConflictSink {
            fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
                assert_ne!(
                    seal.security_id, 13,
                    "conflicting row reached unsafe append"
                );
                self.appended.push(seal.security_id);
                Ok(())
            }
            fn flush(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn discard_pending(&mut self) {}
            fn recover_batch(
                &mut self,
                seals: &[BufferedSeal],
            ) -> Result<RecoveredBatch, RecoveryRefusal> {
                if seals.iter().any(|seal| seal.security_id == 13) {
                    return Err(RecoveryRefusal::Conflict);
                }
                recover_fixture_batch(self, seals)
            }
        }
        let (spill, dlq) = temp_pair("valid-conflict-retention");
        let name = "seals-2026-01-01.cseal4";
        let original = serialized_fixture(13).to_bytes();
        std::fs::write(spill.join(name), original).expect("conflict fixture");
        std::fs::write(
            spill.join("seals-2026-01-02.cseal4"),
            serialized_fixture(14).to_bytes(),
        )
        .expect("independent fixture");
        let mut sink = ConflictSink::default();
        for boot in 0..2 {
            let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
            assert_eq!(outcome.files_deferred_conflict, 1);
            assert_eq!(outcome.files_left_pending, 1);
            assert_eq!(outcome.files_archived, usize::from(boot == 0));
            assert_eq!(outcome.seals_reingested, usize::from(boot == 0));
            assert_eq!(sink.appended, vec![14]);
            assert_eq!(
                std::fs::read(spill.join(SEAL_REPLAYING_SUBDIR).join(name))
                    .expect("unchanged original"),
                original
            );
            assert!(!spill.join(SEAL_ARCHIVE_SUBDIR).join(name).exists());
        }
        cleanup(&spill, &dlq);
    }

    #[test]
    fn direct_production_writer_cannot_replay_without_boot_exclusion() {
        let (spill, dlq) = temp_pair("production-recovery-exclusion");
        let name = "seals-2026-01-01.cseal4";
        let original = serialized_fixture(13).to_bytes();
        std::fs::write(spill.join(name), original).expect("fixture");
        let mut writer = ShadowCandleWriter::for_test();
        let outcome = drain_recovered_seals(&mut writer, &spill, &dlq, 64);
        assert_eq!(outcome.files_deferred_ordering_unverified, 1);
        assert_eq!(outcome.files_archived, 0);
        assert_eq!(outcome.seals_reingested, 0);
        assert_eq!(writer.pending_count(), 0);
        assert_eq!(writer.buffer_byte_count(), 0);
        assert_eq!(
            std::fs::read(spill.join(SEAL_REPLAYING_SUBDIR).join(name)).expect("retained"),
            original
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn retired_timeframe_history_defers_whole_mixed_files_and_replays_separate_active_files() {
        let (spill, dlq) = temp_pair("retired-timeframe-history");
        let binary_name = "seals-retired.cseal3";
        let dlq_name = "seals-retired.cseal3json";
        let retired = [4u8, 6, 8, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21];
        let mut binary = Vec::new();
        let mut json = Vec::new();
        let mut next_id = 1u64;
        // Each supported binary format carries all ten active frames plus
        // all fifteen retired IDs. Retirement is independent of record stride.
        for version in [1u8, 2, 3] {
            for storage_id in (0u8..=24).rev() {
                let mut record = serialized_fixture(next_id);
                next_id += 1;
                record.tf_ordinal = storage_id;
                let mut bytes = record.to_bytes();
                bytes[7] = version;
                let size = if version == 3 {
                    SEAL_SPILL_RECORD_SIZE
                } else {
                    crate::seal_spill::SEAL_SPILL_LEGACY_RECORD_SIZE
                };
                binary.extend_from_slice(&bytes[..size]);
                if version == 3 {
                    json.extend(serde_json::to_vec(&SealDlqRecord::from(&record)).expect("DLQ"));
                    json.push(b'\n');
                }
            }
        }
        std::fs::write(spill.join(binary_name), &binary).expect("mixed binary");
        std::fs::write(dlq.join(dlq_name), &json).expect("mixed DLQ");
        // A separate new 10m file must archive normally in the same pass.
        let new_frame = SerializedSeal::from(&mk_seal(999, 0, TfIndex::M10, 1_716_023_400, 100.0));
        let new_name = "seals-new-10m.cseal3";
        std::fs::write(spill.join(new_name), new_frame.to_bytes()).expect("new 10m");

        let mut sink = StreamingSink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 2);
        assert_eq!(outcome.seals_recovered, TfIndex::ALL.len() * 4 + 1);
        assert_eq!(outcome.seals_reingested, 1);
        assert_eq!(sink.committed, 1);
        assert_eq!(
            sink.committed_id_sum, 999,
            "mixed-file active siblings must never reach the sink"
        );
        assert!(sink.max_pending <= 2);
        assert_eq!(outcome.records_retired_timeframe, retired.len() * 4);
        assert_eq!(
            outcome.records_undecodable, 0,
            "retired history is not corrupt"
        );
        assert_eq!(outcome.seals_left_pending, 100);
        assert_eq!(outcome.files_deferred_retired_timeframe, 2);
        assert!(
            !outcome.pending_count_unknown,
            "fully decoded retired records have an exact known count"
        );
        assert_eq!(outcome.files_left_pending, 2);
        assert_eq!(outcome.files_archived, 1);
        assert_eq!(
            std::fs::read(spill.join(SEAL_REPLAYING_SUBDIR).join(binary_name))
                .expect("retained binary"),
            binary
        );
        assert_eq!(
            std::fs::read(dlq.join(SEAL_REPLAYING_SUBDIR).join(dlq_name)).expect("retained DLQ"),
            json
        );
        assert!(!spill.join(SEAL_ARCHIVE_SUBDIR).join(binary_name).exists());
        assert!(!dlq.join(SEAL_ARCHIVE_SUBDIR).join(dlq_name).exists());
        assert_eq!(
            std::fs::read(spill.join(SEAL_ARCHIVE_SUBDIR).join(new_name)).expect("10m archived"),
            new_frame.to_bytes()
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn unknown_timeframe_is_not_misreported_as_retired_or_reinterpreted_as_an_active_index() {
        for kind in [StagedKind::Spill, StagedKind::Dlq] {
            let mut retired = serialized_fixture(11);
            retired.tf_ordinal = 4; // Historical 1d; runtime index 4 is now 3m.
            let mut unknown = serialized_fixture(12);
            unknown.tf_ordinal = 255;
            let active = SerializedSeal::from(&mk_seal(13, 0, TfIndex::M10, 1_716_023_400, 100.0));
            let mut bytes = Vec::new();
            for record in [retired, unknown, active] {
                match kind {
                    StagedKind::Spill => bytes.extend_from_slice(&record.to_bytes()),
                    StagedKind::Dlq => {
                        bytes.extend(
                            serde_json::to_vec(&SealDlqRecord::from(&record)).expect("DLQ"),
                        );
                        bytes.push(b'\n');
                    }
                }
            }
            let mut reader = BufReader::with_capacity(17, std::io::Cursor::new(bytes));
            let outcome = preflight_staged_stream(&mut reader, kind, Path::new("storage-identity"));
            assert_eq!(outcome.records_decoded, 3);
            assert_eq!(outcome.retired_records, 1);
            assert_eq!(outcome.records_undecodable, 1);
            assert_eq!(outcome.active_records, 1);
            assert!(outcome.pending_unknown());
        }
    }

    #[test]
    fn transient_preflight_read_failure_defers_before_retry_can_reveal_retired_history() {
        for kind in [StagedKind::Spill, StagedKind::Dlq] {
            let active = serialized_fixture(13);
            let mut retired = serialized_fixture(12);
            retired.tf_ordinal = 4;
            let mut bytes = Vec::new();
            let mut fail_at = 0;
            for record in [active, retired] {
                match kind {
                    StagedKind::Spill => bytes.extend_from_slice(&record.to_bytes()),
                    StagedKind::Dlq => {
                        bytes.extend(
                            serde_json::to_vec(&SealDlqRecord::from(&record)).expect("DLQ"),
                        );
                        bytes.push(b'\n');
                    }
                }
                if fail_at == 0 {
                    fail_at = bytes.len();
                }
            }
            let observed = ObservedRead {
                input: std::io::Cursor::new(bytes),
                position: std::rc::Rc::new(std::cell::Cell::new(0)),
                fail_at: Some(fail_at),
            };
            let mut reader = BufReader::with_capacity(1, observed);
            let first =
                preflight_staged_stream(&mut reader, kind, Path::new("transient-preflight"));
            assert_eq!(first.active_records, 1);
            assert_eq!(
                first.retired_records, 0,
                "retirement is still hidden after native read refusal"
            );
            assert!(first.io_failed);
            assert!(first.defers_whole_file());
            assert!(first.pending_unknown());

            // A later read of the same contents succeeds and exposes retirement.
            // The first-pass gate must still refuse any intervening sink append.
            let mut recovered_input = reader.into_inner();
            recovered_input.fail_at = None;
            recovered_input.input.set_position(0);
            recovered_input.position.set(0);
            let mut reader = BufReader::with_capacity(1, recovered_input);
            let mut sink = StreamingSink::default();
            if !first.defers_whole_file() {
                let _ = replay_staged_stream(
                    &mut sink,
                    &mut reader,
                    kind,
                    1,
                    Path::new("unsafe-retry"),
                );
            }
            assert_eq!(sink.appended, 0);
            let later = preflight_staged_stream(&mut reader, kind, Path::new("successful-retry"));
            assert!(later.reached_eof);
            assert_eq!(later.active_records, 1);
            assert_eq!(later.retired_records, 1);
            assert!(!later.io_failed);
            assert!(later.defers_whole_file());
        }
    }

    #[test]
    fn mixed_retired_history_never_overwrites_a_newer_revision_on_first_or_repeated_boot() {
        struct LastWriteWinsSink {
            revision: u64,
            pending: Option<u64>,
            appends: usize,
        }
        impl SealSink for LastWriteWinsSink {
            fn recover_batch(
                &mut self,
                seals: &[BufferedSeal],
            ) -> Result<
                crate::seal_recovery_guard::RecoveredBatch,
                crate::seal_recovery_guard::RecoveryRefusal,
            > {
                crate::seal_writer_task::recover_fixture_batch(self, seals)
            }

            fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
                self.appends += 1;
                if seal.security_id == 13 {
                    self.pending = Some(seal.state.bucket_revision);
                }
                Ok(())
            }
            fn flush(&mut self) -> anyhow::Result<()> {
                if let Some(revision) = self.pending.take() {
                    self.revision = revision;
                }
                Ok(())
            }
            fn discard_pending(&mut self) {
                self.pending = None;
            }
        }
        for kind in [StagedKind::Spill, StagedKind::Dlq] {
            for retired_position in 0..3 {
                let (spill, dlq) =
                    temp_pair(&format!("retired-revision-{kind:?}-{retired_position}"));
                let mut old = serialized_fixture(13);
                old.bucket_revision = 1;
                let mut retired = serialized_fixture(12);
                retired.tf_ordinal = 4;
                let mut records = vec![old, serialized_fixture(14)];
                records.insert(retired_position, retired);
                let mut bytes = Vec::new();
                for record in records {
                    match kind {
                        StagedKind::Spill => bytes.extend_from_slice(&record.to_bytes()),
                        StagedKind::Dlq => {
                            bytes.extend(
                                serde_json::to_vec(&SealDlqRecord::from(&record)).expect("DLQ"),
                            );
                            bytes.push(b'\n');
                        }
                    }
                }
                let (directory, name) = match kind {
                    StagedKind::Spill => (&spill, "seals-retired-revision.cseal3"),
                    StagedKind::Dlq => (&dlq, "seals-retired-revision.cseal3json"),
                };
                std::fs::write(directory.join(name), &bytes).expect("retained history");
                let mut sink = LastWriteWinsSink {
                    revision: 2,
                    pending: None,
                    appends: 0,
                };
                for boot in 0..2 {
                    let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 1);
                    assert_eq!(
                        sink.revision, 2,
                        "boot {boot}: newer correction must survive"
                    );
                    assert_eq!(
                        sink.appends, 0,
                        "whole mixed file must be checked before the first append"
                    );
                    assert_eq!(outcome.seals_recovered, 2);
                    assert_eq!(outcome.seals_reingested, 0);
                    assert_eq!(outcome.seals_left_pending, 3);
                    assert_eq!(outcome.records_retired_timeframe, 1);
                    assert_eq!(outcome.files_deferred_retired_timeframe, 1);
                    assert_eq!(outcome.files_left_pending, 1);
                    assert_eq!(outcome.files_archived, 0);
                    assert!(!outcome.pending_count_unknown);
                    assert_eq!(
                        std::fs::read(directory.join(SEAL_REPLAYING_SUBDIR).join(name))
                            .expect("unchanged original"),
                        bytes
                    );
                }
                cleanup(&spill, &dlq);
            }
        }
    }

    #[test]
    fn damaged_or_unknown_originals_never_write_old_siblings_on_repeated_boots() {
        struct LastWriteWinsSink {
            revision: u64,
            pending: Option<u64>,
            appends: usize,
            flushes: usize,
        }
        impl SealSink for LastWriteWinsSink {
            fn recover_batch(
                &mut self,
                seals: &[BufferedSeal],
            ) -> Result<
                crate::seal_recovery_guard::RecoveredBatch,
                crate::seal_recovery_guard::RecoveryRefusal,
            > {
                crate::seal_writer_task::recover_fixture_batch(self, seals)
            }

            fn append_seal(&mut self, seal: &BufferedSeal) -> anyhow::Result<()> {
                self.appends += 1;
                self.pending = Some(seal.state.bucket_revision);
                Ok(())
            }
            fn flush(&mut self) -> anyhow::Result<()> {
                self.flushes += 1;
                if let Some(revision) = self.pending.take() {
                    self.revision = revision;
                }
                Ok(())
            }
            fn discard_pending(&mut self) {
                self.pending = None;
            }
        }
        fn encode(kind: StagedKind, record: &SerializedSeal) -> Vec<u8> {
            match kind {
                StagedKind::Spill => record.to_bytes().to_vec(),
                StagedKind::Dlq => {
                    let mut bytes = serde_json::to_vec(&SealDlqRecord::from(record)).expect("DLQ");
                    bytes.push(b'\n');
                    bytes
                }
            }
        }

        let mut old = serialized_fixture(11);
        old.bucket_revision = 1;
        let mut unknown = old;
        unknown.tf_ordinal = 255;
        for kind in [StagedKind::Spill, StagedKind::Dlq] {
            let active_bytes = encode(kind, &old);
            // The bool marks a torn tail, which is deliberately placed only
            // at EOF. Other malformed records exercise start/middle/end.
            let mut malformed = vec![("unknown-timeframe", encode(kind, &unknown), false)];
            match kind {
                StagedKind::Spill => {
                    let mut bad_version = active_bytes.clone();
                    bad_version[7] = 0;
                    malformed.push(("unsupported-version", bad_version, false));
                    let mut bad_classification = active_bytes.clone();
                    bad_classification[161] = 2;
                    malformed.push(("invalid-classification", bad_classification, false));
                    malformed.push(("torn-header", active_bytes[..7].to_vec(), true));
                    malformed.push(("torn-record", active_bytes[..11].to_vec(), true));
                }
                StagedKind::Dlq => {
                    malformed.push(("invalid-json", b"{not-json}\n".to_vec(), false));
                    malformed.push(("invalid-utf8", b"{\xff}\n".to_vec(), false));
                    let mut oversized = vec![b'x'; SEAL_DLQ_RECORD_MAX_BYTES + 1];
                    oversized.push(b'\n');
                    malformed.push(("oversized-line", oversized, false));
                    malformed.push(("torn-json", b"{\"security_id\":".to_vec(), true));
                }
            }
            for (tag, bad, tail_only) in malformed {
                for position in 0..=2 {
                    if tail_only && position != 2 {
                        continue;
                    }
                    let (spill, dlq) =
                        temp_pair(&format!("unverified-original-{kind:?}-{tag}-{position}"));
                    let mut original = Vec::new();
                    for item in 0..=2 {
                        original.extend_from_slice(if item == position {
                            &bad
                        } else {
                            &active_bytes
                        });
                    }
                    let (directory, name) = match kind {
                        StagedKind::Spill => (&spill, "seals-unverified.cseal3"),
                        StagedKind::Dlq => (&dlq, "seals-unverified.cseal3json"),
                    };
                    std::fs::write(directory.join(name), &original).expect("damaged original");
                    let mut sink = LastWriteWinsSink {
                        revision: 2,
                        pending: None,
                        appends: 0,
                        flushes: 0,
                    };
                    for boot in 0..3 {
                        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 1);
                        assert_eq!(
                            sink.revision, 2,
                            "{kind:?} {tag} {position}, boot {boot}: old siblings cannot overwrite a newer correction"
                        );
                        assert_eq!(sink.appends, 0);
                        assert_eq!(sink.flushes, 0);
                        assert_eq!(sink.pending, None);
                        assert_eq!(outcome.seals_reingested, 0);
                        assert_eq!(outcome.files_archived, 0);
                        assert_eq!(outcome.files_left_pending, 1);
                        assert_eq!(outcome.files_deferred_invalid, 1);
                        assert_eq!(outcome.files_deferred_retired_timeframe, 0);
                        assert!(outcome.records_undecodable > 0);
                        assert!(outcome.pending_count_unknown);
                        assert_eq!(
                            std::fs::read(directory.join(SEAL_REPLAYING_SUBDIR).join(name))
                                .expect("whole original retained"),
                            original
                        );
                        assert!(!directory.join(SEAL_ARCHIVE_SUBDIR).join(name).exists());
                    }
                    cleanup(&spill, &dlq);
                }
            }
        }
    }

    #[test]
    fn replay_pass_flushes_many_batches_before_its_eof_with_bounded_sink() {
        use std::io::Write;
        let (spill, dlq) = temp_pair("streaming-many-batches");
        let path = spill.join("seals-large.cseal3");
        let records = 20_003usize;
        let mut output = std::fs::File::create(&path).expect("large fixture");
        // This fixture measures the bounded replay pass only. Production
        // first preflights the whole file, then rewinds before this pass.
        // Neither pass needs a record vector proportional to file length.
        for id in 1..=records as u64 {
            output
                .write_all(&serialized_fixture(id).to_bytes())
                .expect("write fixture record");
        }
        drop(output);
        for requested_batch in [0usize, 37] {
            let position = std::rc::Rc::new(std::cell::Cell::new(0));
            let observed = ObservedRead {
                input: std::fs::File::open(&path).expect("fixture input"),
                position: std::rc::Rc::clone(&position),
                fail_at: None,
            };
            // One-byte buffering makes read progress exact at each complete
            // binary record instead of hiding up to an OS-reader buffer.
            let mut reader = BufReader::with_capacity(1, observed);
            let mut sink = StreamingSink {
                read_position: Some(position),
                ..Default::default()
            };
            let outcome = replay_staged_stream(
                &mut sink,
                &mut reader,
                StagedKind::Spill,
                requested_batch,
                &path,
            );
            let batch = requested_batch.max(1);
            assert!(outcome.complete());
            assert_eq!(outcome.seals_reingested, records);
            assert_eq!(sink.committed, records);
            assert_eq!(
                sink.committed_id_sum,
                records as u64 * (records as u64 + 1) / 2
            );
            assert_eq!(sink.max_pending, batch);
            assert_eq!(sink.flushes, records.div_ceil(batch));
            assert_eq!(
                sink.first_flush_position,
                Some(batch * SEAL_SPILL_RECORD_SIZE)
            );
            assert!(sink.first_flush_position.unwrap() < records * SEAL_SPILL_RECORD_SIZE);
            assert_eq!(sink.pending, 0);
            assert_eq!(
                std::fs::metadata(&path).expect("original retained").len(),
                (records * SEAL_SPILL_RECORD_SIZE) as u64
            );
        }
        cleanup(&spill, &dlq);
    }

    #[test]
    fn preflight_late_corruption_defers_whole_original_and_recovers_later_file() {
        let (spill, dlq) = temp_pair("streaming-late-corruption");
        let name = "seals-2026-01-01.cseal3";
        let mut original = Vec::new();
        for id in 1..=13u64 {
            original.extend_from_slice(&serialized_fixture(id).to_bytes());
        }
        original.extend_from_slice(&serialized_fixture(14).to_bytes()[..11]);
        std::fs::write(spill.join(name), &original).expect("late torn tail");
        std::fs::write(
            spill.join("seals-2026-01-02.cseal3"),
            serialized_fixture(50).to_bytes(),
        )
        .expect("later healthy file");
        let mut sink = StreamingSink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 4);
        assert_eq!(
            sink.committed, 1,
            "only the separate healthy file can reach the sink"
        );
        assert_eq!(sink.committed_id_sum, 50);
        assert_eq!(sink.max_pending, 1);
        assert_eq!(
            outcome.seals_recovered, 14,
            "thirteen deferred valid siblings plus one healthy seal"
        );
        assert_eq!(
            outcome.seals_left_pending, 13,
            "the malformed boundary hides an unknown tail"
        );
        assert_eq!(outcome.seals_reingested, 1);
        assert_eq!(outcome.records_undecodable, 1);
        assert_eq!(outcome.files_deferred_invalid, 1);
        assert_eq!(outcome.files_left_pending, 1);
        assert_eq!(outcome.files_archived, 1);
        assert!(outcome.pending_count_unknown);
        assert_eq!(
            std::fs::read(spill.join(SEAL_REPLAYING_SUBDIR).join(name))
                .expect("whole original retained"),
            original
        );
        assert!(!spill.join(SEAL_ARCHIVE_SUBDIR).join(name).exists());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn streaming_sink_refusal_stops_before_unread_tail_and_preserves_originals() {
        for append_failure in [false, true] {
            let (spill, dlq) = temp_pair(&format!("streaming-sink-refusal-{append_failure}"));
            let name = "seals-2026-01-01.cseal3";
            let original: Vec<u8> = (1..=20u64)
                .flat_map(|id| serialized_fixture(id).to_bytes())
                .collect();
            let later = serialized_fixture(100).to_bytes();
            std::fs::write(spill.join(name), &original).expect("first fixture");
            std::fs::write(spill.join("seals-2026-01-02.cseal3"), later).expect("later fixture");
            let mut sink = StreamingSink {
                fail_append: append_failure.then_some(6),
                fail_flush: (!append_failure).then_some(2),
                ..Default::default()
            };
            let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 4);
            let decoded = 8; // admission now waits for the complete bounded batch
            assert_eq!(outcome.seals_recovered, decoded);
            assert_eq!(
                outcome.seals_reingested, 4,
                "only the first confirmed batch counts"
            );
            assert_eq!(
                outcome.seals_left_pending,
                decoded - 4,
                "known decoded pending only"
            );
            assert!(outcome.pending_count_unknown);
            assert_eq!(outcome.files_left_pending, 2);
            assert_eq!(outcome.files_archived, 0);
            assert_eq!(
                sink.appended,
                if append_failure { 6 } else { 8 },
                "neither unread tail nor later file reached the failed sink"
            );
            assert_eq!(sink.discards, 1);
            assert_eq!(sink.pending, 0);
            assert_eq!(sink.committed_id_sum, 1 + 2 + 3 + 4);
            assert_eq!(
                std::fs::read(spill.join(SEAL_REPLAYING_SUBDIR).join(name))
                    .expect("original still replayable"),
                original
            );
            assert_eq!(
                std::fs::read(
                    spill
                        .join(SEAL_REPLAYING_SUBDIR)
                        .join("seals-2026-01-02.cseal3")
                )
                .expect("unread original retained"),
                later
            );
            cleanup(&spill, &dlq);
        }
    }

    #[test]
    fn second_pass_read_error_discards_unflushed_prefix_without_claiming_eof() {
        let bytes: Vec<u8> = (1..=20u64)
            .flat_map(|id| serialized_fixture(id).to_bytes())
            .collect();
        let fail_at = 5 * SEAL_SPILL_RECORD_SIZE + 11;
        let position = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = ObservedRead {
            input: std::io::Cursor::new(&bytes),
            position: std::rc::Rc::clone(&position),
            fail_at: Some(fail_at),
        };
        let mut reader = BufReader::with_capacity(1, observed);
        let mut sink = StreamingSink::default();
        let outcome = replay_staged_stream(
            &mut sink,
            &mut reader,
            StagedKind::Spill,
            4,
            Path::new("read-error-fixture"),
        );
        assert_eq!(
            position.get(),
            fail_at,
            "the persistent read error must not be polled forever"
        );
        assert_eq!(
            outcome.seals_reingested, 4,
            "earlier acknowledged batches remain applied"
        );
        assert_eq!(sink.committed_id_sum, 1 + 2 + 3 + 4);
        assert_eq!(
            sink.discards, 1,
            "the current unflushed prefix must stay pending"
        );
        assert_eq!(sink.pending, 0);
        assert_eq!(sink.max_pending, 4);
        assert_eq!(outcome.records_undecodable, 1);
        assert!(!outcome.reached_eof);
        assert!(!outcome.complete());
        assert!(outcome.pending_unknown());
        assert!(
            !outcome.sink_failed,
            "a damaged source is distinct from a failed database"
        );
    }

    #[test]
    fn bounded_dlq_preflight_defers_valid_siblings_of_oversized_input() {
        let (spill, dlq) = temp_pair("streaming-dlq-line-cap");
        let mut bytes = serde_json::to_vec(&SealDlqRecord::from(&serialized_fixture(11)))
            .expect("first record");
        assert!(bytes.len() < SEAL_DLQ_RECORD_MAX_BYTES);
        bytes.push(b'\n');
        // Deliberately much larger than the permitted record; the decoder
        // must skip to newline without constructing a String of this size.
        bytes.extend(std::iter::repeat_n(b'x', SEAL_DLQ_RECORD_MAX_BYTES * 8));
        bytes.push(b'\n');
        bytes.extend(
            serde_json::to_vec(&SealDlqRecord::from(&serialized_fixture(25))).expect("last record"),
        );
        // The final valid record intentionally has no trailing newline.
        let name = "seals-large-line.cseal3json";
        std::fs::write(dlq.join(name), &bytes).expect("mixed DLQ fixture");
        let mut sink = StreamingSink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 1);
        assert_eq!(sink.appended, 0);
        assert_eq!(sink.committed, 0);
        assert_eq!(sink.committed_id_sum, 0);
        assert_eq!(sink.max_pending, 0);
        assert_eq!(
            outcome.seals_recovered, 2,
            "bounded inspection can count both valid siblings"
        );
        assert_eq!(outcome.seals_left_pending, 2);
        assert_eq!(outcome.records_undecodable, 1);
        assert_eq!(outcome.files_deferred_invalid, 1);
        assert!(outcome.pending_count_unknown);
        assert_eq!(outcome.files_left_pending, 1);
        assert_eq!(outcome.files_archived, 0);
        assert_eq!(
            std::fs::read(dlq.join(SEAL_REPLAYING_SUBDIR).join(name))
                .expect("oversized original retained"),
            bytes
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn bounded_dlq_line_limit_is_exact_across_reader_boundaries() {
        for length in [
            SEAL_DLQ_RECORD_MAX_BYTES - 1,
            SEAL_DLQ_RECORD_MAX_BYTES,
            SEAL_DLQ_RECORD_MAX_BYTES + 1,
        ] {
            let mut bytes = vec![b'x'; length];
            bytes.extend_from_slice(b"\n{}\r\n");
            let mut reader = BufReader::with_capacity(17, std::io::Cursor::new(bytes));
            let mut line = [0u8; SEAL_DLQ_RECORD_MAX_BYTES];
            assert_eq!(
                read_bounded_dlq_line(&mut reader, &mut line).expect("bounded first line"),
                Some((
                    length.min(SEAL_DLQ_RECORD_MAX_BYTES),
                    length > SEAL_DLQ_RECORD_MAX_BYTES
                ))
            );
            assert_eq!(
                read_bounded_dlq_line(&mut reader, &mut line).expect("following complete line"),
                Some((3, false))
            );
            assert_eq!(&line[..3], b"{}\r");
            assert_eq!(
                read_bounded_dlq_line(&mut reader, &mut line).expect("clean EOF"),
                None
            );
        }
    }

    #[test]
    fn exhausted_staging_and_archive_names_preserve_every_original() {
        for have_overflow in [false, true] {
            let (spill, dlq) = temp_pair(&format!("name-exhaustion-{have_overflow}"));
            let replaying = spill.join(SEAL_REPLAYING_SUBDIR);
            let archive = spill.join(SEAL_ARCHIVE_SUBDIR);
            std::fs::create_dir_all(&replaying).expect("staging dir");
            std::fs::create_dir_all(&archive).expect("archive dir");
            let sentinel = dlq.join("occupied-original");
            std::fs::write(&sentinel, b"retained destination bytes").expect("sentinel");

            for (directory, name) in [
                (&replaying, "seals-stage.cseal3"),
                (&archive, "seals-archive.cseal3"),
            ] {
                for suffix in 0..crate::seal_spill::SEAL_MOVE_NAME_ATTEMPTS {
                    let name = if suffix == 0 {
                        name.to_string()
                    } else {
                        format!("{name}.{suffix}")
                    };
                    std::fs::hard_link(&sentinel, directory.join(name)).expect("occupied name");
                }
                if have_overflow {
                    std::fs::hard_link(&sentinel, directory.join(format!("{name}.overflow")))
                        .expect("historical overflow sentinel");
                }
            }

            let live = spill.join("seals-stage.cseal3");
            let staged = replaying.join("seals-archive.cseal3");
            std::fs::write(&live, b"live original").expect("live source");
            std::fs::write(&staged, b"confirmed original").expect("archive source");
            assert!(stage_pending_files(&spill).is_err());
            assert!(archive_staged(&staged).is_err());
            assert_eq!(
                std::fs::read(&live).expect("live retained"),
                b"live original"
            );
            assert_eq!(
                std::fs::read(&staged).expect("staged retained"),
                b"confirmed original"
            );
            assert_eq!(
                std::fs::read(&sentinel).expect("all occupied hard links retained"),
                b"retained destination bytes"
            );
            assert_eq!(
                replaying.join("seals-stage.cseal3.overflow").exists(),
                have_overflow
            );
            assert_eq!(
                archive.join("seals-archive.cseal3.overflow").exists(),
                have_overflow
            );
            cleanup(&spill, &dlq);
        }
    }

    #[test]
    fn move_seal_file_no_replace_handles_concurrent_collisions_without_overwrite() {
        let (spill, dlq) = temp_pair("concurrent-names");
        let mut sources = Vec::new();
        for id in 0..8u8 {
            let parent = spill.join(id.to_string());
            std::fs::create_dir_all(&parent).expect("source dir");
            let source = parent.join("seals-shared.cseal3");
            std::fs::write(&source, [id]).expect("source bytes");
            sources.push(source);
        }
        let barrier = std::sync::Barrier::new(sources.len());
        std::thread::scope(|scope| {
            for source in &sources {
                let destination = &dlq;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    crate::seal_spill::move_seal_file_no_replace(source, destination)
                        .expect("unique no-replace destination");
                });
            }
        });
        let paths = list_seal_files(&dlq, false).expect("all destinations discoverable");
        let mut values: Vec<_> = paths
            .iter()
            .map(|path| std::fs::read(path).expect("retained bytes")[0])
            .collect();
        values.sort_unstable();
        assert_eq!(values, (0..8u8).collect::<Vec<_>>());
        assert!(sources.iter().all(|path| !path.exists()));
        assert!(staged_kind(Path::new("seals-day.cseal3.1.2")).is_some());
        assert!(staged_kind(Path::new("seals-day.cseal3.overflow.1")).is_some());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn directory_and_entry_failures_never_report_a_clean_empty_backlog() {
        let (spill, dlq) = temp_pair("discovery-errors");
        let live = spill.join("seals-day.cseal3");
        std::fs::write(&live, b"original bytes").expect("source");
        let entry = std::fs::read_dir(&spill)
            .expect("real directory")
            .next()
            .expect("entry");
        let injected = vec![
            entry,
            Err(std::io::Error::other("injected directory entry failure")),
        ];
        assert!(collect_seal_files(injected).is_err());
        assert_eq!(
            std::fs::read(&live).expect("untouched source"),
            b"original bytes"
        );
        assert!(!spill.join(SEAL_REPLAYING_SUBDIR).exists());

        // A regular file where a directory is configured is a deterministic
        // inspection failure even when tests run with elevated privileges.
        let blocked = dlq.join("not-a-directory");
        std::fs::write(&blocked, b"retained blocker").expect("blocker");
        let mut sink = RecordingSink::default();
        let outcome = drain_recovered_seals(&mut sink, &blocked, &blocked, 64);
        assert_eq!(outcome.discovery_errors, 1);
        assert!(outcome.pending_count_unknown);
        assert!(!outcome.is_clean());
        assert_eq!(
            outcome.files_left_pending, 0,
            "an unknown count must not be fabricated"
        );
        assert!(sink.accepted.is_empty());
        assert_eq!(
            std::fs::read(&blocked).expect("blocker retained"),
            b"retained blocker"
        );

        let absent = dlq.join("genuinely-absent");
        assert!(drain_recovered_seals(&mut sink, &absent, &absent, 64).is_clean());
        cleanup(&spill, &dlq);
    }

    #[test]
    fn unreadable_staging_directory_refuses_before_moving_live_files() {
        let (spill, dlq) = temp_pair("staging-blocked");
        let live = spill.join("seals-day.cseal3");
        std::fs::write(&live, b"pending live bytes").expect("live");
        std::fs::write(spill.join(SEAL_REPLAYING_SUBDIR), b"staging blocker").expect("blocker");
        let mut sink = RecordingSink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(outcome.discovery_errors, 1);
        assert!(outcome.pending_count_unknown);
        assert!(!outcome.is_clean());
        assert!(sink.accepted.is_empty());
        assert_eq!(
            std::fs::read(&live).expect("live retained"),
            b"pending live bytes"
        );
        cleanup(&spill, &dlq);
    }

    #[test]
    fn a_damaged_original_is_deferred_without_stranding_separate_complete_files() {
        let (spill, dlq) = temp_pair("prefix-salvage");
        let first = SerializedSeal::from(&mk_seal(11, 0, TfIndex::M1, 1_716_023_700, 100.0));
        let interrupted = SerializedSeal::from(&mk_seal(22, 0, TfIndex::M1, 1_716_023_700, 100.0));
        let later = SerializedSeal::from(&mk_seal(33, 0, TfIndex::M1, 1_716_023_700, 100.0));
        let mut original = first.to_bytes().to_vec();
        original.extend_from_slice(&interrupted.to_bytes()[..17]);
        std::fs::write(spill.join("seals-2026-01-01.cseal3"), &original).expect("torn source");
        std::fs::write(spill.join("seals-2026-01-02.cseal3"), later.to_bytes())
            .expect("healthy source");
        let mut sink = RecordingSink::default();
        let outcome = drain_recovered_seals(&mut sink, &spill, &dlq, 64);
        assert_eq!(
            sink.accepted,
            vec![33],
            "the old prefix must never reach the sink"
        );
        assert_eq!(outcome.seals_recovered, 2);
        assert_eq!(outcome.seals_left_pending, 1);
        assert_eq!(outcome.seals_reingested, 1);
        assert_eq!(outcome.records_undecodable, 1);
        assert_eq!(outcome.files_deferred_invalid, 1);
        assert_eq!(outcome.files_left_pending, 1);
        assert_eq!(outcome.files_archived, 1);
        assert!(outcome.pending_count_unknown);
        assert_eq!(
            std::fs::read(
                spill
                    .join(SEAL_REPLAYING_SUBDIR)
                    .join("seals-2026-01-01.cseal3")
            )
            .expect("whole torn original retained"),
            original
        );
        assert!(
            spill
                .join(SEAL_ARCHIVE_SUBDIR)
                .join("seals-2026-01-02.cseal3")
                .exists()
        );
        cleanup(&spill, &dlq);
    }
}
