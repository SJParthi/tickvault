//! Wave 6 Sub-PR #1 item 1.2d — sealed-candle NDJSON dead-letter queue.
//!
//! Third absorption tier of the locked Wave 6 design L-C1:
//! `SealRing` (in-memory FIFO) → `SealSpillWriter` (binary 128-byte
//! disk records) → `SealDlqWriter` (this module — NDJSON last-resort).
//!
//! ## Why NDJSON for the third tier
//!
//! - **Recoverable text**. The binary spill file is exact + compact
//!   but operator-opaque. Once we are routing seals to the DLQ the
//!   normal pipeline is already broken; the operator needs to be
//!   able to `cat data/dlq/seals_v4-YYYY-MM-DD.ndjson | jq` to inspect
//!   what was lost. NDJSON is the canonical text-streaming format
//!   matching the existing `data/logs/errors.jsonl.*` rotation.
//! - **Append-only single-line records.** A partial trailing line on
//!   crash is silently dropped during replay (the `serde_json::from_str`
//!   on the corrupt line returns `Err` and the reader skips it).
//! - **Forward-compatible.** Adding a new field to `SealDlqRecord`
//!   produces a JSON object with one extra key; older replay code
//!   that doesn't know about it ignores via `#[serde(default)]`.
//!
//! ## What this module ships
//!
//! - [`SealDlqRecord`] — serde-serialisable mirror of
//!   `SerializedSeal` (the binary wire-format used by the spill tier).
//!   Lossless `From<&SerializedSeal>` + `From<&SealDlqRecord> for
//!   SerializedSeal` so the writer task can convert from any tier
//!   without re-deriving the trading-side `BufferedSeal`.
//! - [`SealDlqWriter`] — append-only NDJSON file writer with the
//!   exact `seal_spill.rs` API surface:
//!   - IST-date file rotation (`seals_v4-2026-05-10.ndjson`).
//!   - `append_record()` (one line per call).
//!   - `read_all()` recovery scan that silently drops corrupt
//!     lines with `warn!` so a single bad line does NOT stall replay.
//!   - `clear_dlq_for_date()` idempotent removal.
//!   - `with_dlq_dir_for_test()` for parallel test isolation.
//!
//! ## When this fires
//!
//! Per locked decision L-C1, the writer task escalates to this DLQ
//! ONLY when the in-memory ring AND the binary spill BOTH fail. If
//! the DLQ append also fails the writer task logs
//! `error!(code = ErrorCode::AggregatorDrop01.code_str())` per the
//! AGGREGATOR-DROP-01 runbook in
//! `.claude/rules/project/wave-6-error-codes.md`. Triple-tier failure
//! = operator-action-required; the host is OOM AND out of disk AND
//! the `data/dlq/` directory is unwritable.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;

use crate::seal_spill::{
    SEAL_SPILL_FORMAT_VERSION, SerializedSeal, ist_day_number, seal_spill_version_is_readable,
    sync_directory,
};

/// Production DLQ directory — sibling of `data/spill/` so operators
/// looking at `data/` see all three absorption tiers next to each
/// other (`logs/`, `spill/`, `dlq/`).
const SEAL_DLQ_DIR: &str = "data/dlq";

/// JSON-serialisable mirror of [`SerializedSeal`]. Field names are
/// stable wire format for `jq` operability — every new field MUST
/// be added with `#[serde(default)]` so older replay code that
/// doesn't know about it skips it without error.
///
/// Field-by-field correspondence with `SerializedSeal`:
/// - `security_id`, `exchange_segment_code` — composite key (I-P1-11).
/// - `tf_ordinal` — `TfIndex::as_ordinal()` (0..`TF_COUNT`; nine frames since
///   2026-09-19). Its MEANING depends on `format_version` — see that field.
/// - `bucket_start_ist_secs`, `tick_count`, `volume`,
///   `bucket_start_cumulative`, `oi`, `open`, `high`, `low`, `close`
///   — `LiveCandleState` payload.
/// - `close_pct_from_prev_day`, `oi_pct_from_prev_day`,
///   `volume_pct_from_prev_day` — Wave-5 stamped-at-seal pct fields
///   (per locked decision L-H6).
// `Copy` dropped 2026-06-23: the `feed: String` field (round-trips broker
// provenance through the DLQ NDJSON) makes the record non-`Copy`. The DLQ path
// is the cold last-resort tier (NDJSON serialize/deserialize), never the hot
// path, so a `Clone`-only record is fine — every conversion uses `&self`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SealDlqRecord {
    /// The seal-record format this line was written under — the SAME number
    /// as byte 7 of a binary spill record, [`SEAL_SPILL_FORMAT_VERSION`],
    /// because the two tiers carry the same `tf_ordinal` space.
    ///
    /// Added 2026-09-22. Until then the DLQ carried NO version at all, so a
    /// line written under the 24-frame ordinal space (before 2026-09-19)
    /// decoded silently into the nine-frame space: `tf_ordinal` 5 was `S1`
    /// and is now `M30`, so a one-second bar was re-ingested as a thirty-minute
    /// bar with nothing in any log to say so. The binary spill had a version
    /// byte for exactly this reason; the NDJSON sibling never did.
    ///
    /// `#[serde(default)]` makes every pre-2026-09-22 line read as `0`, which
    /// is below every real version, so readers REFUSE it — the only safe
    /// reading of a record whose ordinal space cannot be known.
    #[serde(default)]
    pub format_version: u8,
    // `u64` (2026-06-29 widening) — the seal DLQ carries BOTH Dhan (≤u32) and
    // Groww (bit-62 index ids > u32) seals; NDJSON round-trips u64 natively.
    #[serde(default)]
    pub security_id: u64,
    #[serde(default)]
    pub exchange_segment_code: u8,
    #[serde(default)]
    pub tf_ordinal: u8,
    #[serde(default)]
    pub bucket_start_ist_secs: u32,
    #[serde(default)]
    pub tick_count: u32,
    #[serde(default)]
    pub volume: u64,
    #[serde(default)]
    pub bucket_start_cumulative: u64,
    #[serde(default)]
    pub oi: i64,
    #[serde(default)]
    pub open: f64,
    #[serde(default)]
    pub high: f64,
    #[serde(default)]
    pub low: f64,
    #[serde(default)]
    pub close: f64,
    #[serde(default)]
    pub close_pct_from_prev_day: f64,
    /// The PREVIOUS sealed bar's close — the baseline the persisted `volume`'s
    /// SIGN is measured against (`LiveCandleState::signed_volume`).
    ///
    /// This key carried the same meaning before 2026-09-10, when it was
    /// replaced by `net_volume_signed` / `net_volume_classified` for the
    /// tick-rule flow scheme; the 2026-09-18 directive retired that scheme and
    /// the key is back. Both of those names are simply absent from records
    /// this writer produces, and `serde` ignores them on the way in, so an
    /// NDJSON file from either era decodes without error.
    ///
    /// `0.0` means "no baseline" — the session's first bucket, or a record
    /// written by a version that did not carry it. `signed_volume()` reports
    /// such a bar POSITIVE, which is the documented default rather than a
    /// claim that the close rose.
    #[serde(default)]
    pub bucket_open_prev_close: f64,
    #[serde(default)]
    pub total_buy_qty: u32,
    #[serde(default)]
    pub total_sell_qty: u32,
    /// §31 Option 2 (2026-06-01): % vs the official 09:15 session open.
    #[serde(default)]
    pub open_pct: f64,
    /// Operator request 2026-06-02: headline day change % (close vs
    /// yesterday's close).
    #[serde(default)]
    pub change_pct: f64,
    /// Operator request 2026-06-02: opening gap % (today's 09:15 open vs
    /// yesterday's close).
    #[serde(default)]
    pub open_gap_pct: f64,
    /// Broker-source feed label (`"dhan"` / `"groww"`). `#[serde(default)]` ⇒
    /// pre-feed DLQ NDJSON records (no `feed` key) deserialise to `""`, which
    /// the `→ SerializedSeal` conversion maps to `Feed::Dhan` — backward-compatible.
    /// Round-trips the feed through the DLQ so a Groww seal recovered from the
    /// last-resort NDJSON still writes `feed='groww'`.
    #[serde(default)]
    pub feed: String,
}

impl From<&SerializedSeal> for SealDlqRecord {
    /// Lossless conversion from the binary spill record to the
    /// JSON-serialisable DLQ record. `O(1)`, zero allocation
    /// (`Copy` of fixed-size primitive fields).
    #[inline]
    fn from(s: &SerializedSeal) -> Self {
        Self {
            // Stamped on EVERY write, so every line this build produces is
            // readable by this build and refused by any build whose ordinal
            // space differs.
            format_version: SEAL_SPILL_FORMAT_VERSION,
            security_id: s.security_id,
            exchange_segment_code: s.exchange_segment_code,
            // Round-trip feed provenance through the DLQ NDJSON.
            feed: s.feed.as_str().to_string(),
            tf_ordinal: s.tf_ordinal,
            bucket_start_ist_secs: s.bucket_start_ist_secs,
            tick_count: s.tick_count,
            volume: s.volume,
            bucket_start_cumulative: s.bucket_start_cumulative,
            oi: s.oi,
            open: s.open,
            high: s.high,
            low: s.low,
            close: s.close,
            close_pct_from_prev_day: s.close_pct_from_prev_day,
            bucket_open_prev_close: s.bucket_open_prev_close,
            total_buy_qty: s.total_buy_qty,
            total_sell_qty: s.total_sell_qty,
            open_pct: s.open_pct,
            change_pct: s.change_pct,
            open_gap_pct: s.open_gap_pct,
        }
    }
}

impl From<&SealDlqRecord> for SerializedSeal {
    /// Reverse direction — used by the writer task on REPLAY to
    /// re-attempt the ILP send via the existing binary path. Lossless
    /// (every field copied 1:1).
    #[inline]
    fn from(r: &SealDlqRecord) -> Self {
        Self {
            security_id: r.security_id,
            exchange_segment_code: r.exchange_segment_code,
            // Pre-feed DLQ records have feed="" → Feed::Dhan (backward-compatible);
            // an unknown label also degrades to Dhan rather than panicking.
            feed: Feed::parse(&r.feed).unwrap_or(Feed::Dhan),
            tf_ordinal: r.tf_ordinal,
            bucket_start_ist_secs: r.bucket_start_ist_secs,
            tick_count: r.tick_count,
            volume: r.volume,
            bucket_start_cumulative: r.bucket_start_cumulative,
            oi: r.oi,
            open: r.open,
            high: r.high,
            low: r.low,
            close: r.close,
            close_pct_from_prev_day: r.close_pct_from_prev_day,
            bucket_open_prev_close: r.bucket_open_prev_close,
            total_buy_qty: r.total_buy_qty,
            total_sell_qty: r.total_sell_qty,
            open_pct: r.open_pct,
            change_pct: r.change_pct,
            open_gap_pct: r.open_gap_pct,
        }
    }
}

/// Returns today's IST date in `seals_v4-YYYY-MM-DD.ndjson` form for the
/// DLQ filename. Pure function for testability (clock injected by
/// caller in tests). Mirrors `seal_spill::ist_date_filename` but with
/// the `.ndjson` suffix.
fn ist_date_filename(now_unix_secs: i64) -> String {
    // `IST_UTC_OFFSET_SECONDS` per data-integrity.md — 19_800.
    let ist_secs = now_unix_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let dt = Utc
        .timestamp_opt(ist_secs, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap_or_default());
    dt.format("seals_v4-%Y-%m-%d.ndjson").to_string()
}

/// Counter for a failed DLQ sync, one series per `reason`.
pub const SEAL_DLQ_SYNC_FAILED_COUNTER: &str = "tv_seal_dlq_sync_failed_total";

/// Append-only NDJSON DLQ writer. One instance lives in the writer
/// task; `append_record` is the single producer entry point.
pub struct SealDlqWriter {
    /// DLQ directory — production uses `SEAL_DLQ_DIR`; tests
    /// override via `with_dlq_dir_for_test`.
    dlq_dir: PathBuf,
    /// Day files written since the last [`Self::sync_written`], and whether
    /// a directory entry was created (2026-10-04: the DLQ was flushed to the
    /// page cache and never synced, so a power cut could take records it had
    /// reported written).
    unsynced: Mutex<DlqUnsynced>,
    /// Edge latch for the sync-failure `error!`: one line per failing
    /// episode, cleared by the next clean sync.
    sync_failing: AtomicBool,
}

/// What [`SealDlqWriter::sync_written`] still has to sync. Two day slots
/// suffice: the writers sync at least once a second while the DLQ is
/// written, so only the IST midnight can separate two days between syncs. A
/// third day before any sync replaces the older slot and is counted
/// (`reason="slot_overflow"`); that day's file is then synced by nobody.
#[derive(Default)]
struct DlqUnsynced {
    /// `(IST day number, a unix second inside that day)` per written day.
    days: [Option<(i64, i64)>; 2],
    /// A new day file was created: the DLQ directory entry needs a sync.
    dir: bool,
    /// The DLQ directory itself was created: its parent's entry too.
    parent: bool,
}

impl DlqUnsynced {
    /// Records a write to the day holding `now_unix_secs`. O(1).
    fn note_day(&mut self, now_unix_secs: i64) {
        let day = ist_day_number(now_unix_secs);
        if self.days.iter().flatten().any(|(held, _)| *held == day) {
            return;
        }
        if let Some(free) = self.days.iter_mut().find(|slot| slot.is_none()) {
            *free = Some((day, now_unix_secs));
            return;
        }
        metrics::counter!(SEAL_DLQ_SYNC_FAILED_COUNTER, "reason" => "slot_overflow").increment(1);
        let oldest = usize::from(self.days[1].map(|(d, _)| d) < self.days[0].map(|(d, _)| d));
        self.days[oldest] = Some((day, now_unix_secs));
    }
}

impl SealDlqWriter {
    /// Production constructor. Uses `data/dlq/`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_dlq_dir_for_test(PathBuf::from(SEAL_DLQ_DIR))
    }

    /// Test constructor. Tests pass an isolated `tempdir` to allow
    /// parallel execution.
    #[must_use]
    // TEST-EXEMPT: test-only helper used as construction source by every test in this module (test_append_record_then_read_all_roundtrip, test_seal_dlq_writer_clear_*, test_seal_dlq_writer_skips_corrupt_lines_gracefully, etc.). Separate name-matched test would be redundant.
    pub fn with_dlq_dir_for_test(dir: PathBuf) -> Self {
        Self {
            dlq_dir: dir,
            unsynced: Mutex::new(DlqUnsynced::default()),
            sync_failing: AtomicBool::new(false),
        }
    }

    fn lock_unsynced(&self) -> std::sync::MutexGuard<'_, DlqUnsynced> {
        self.unsynced
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Test probe: is any DLQ write or created entry still unsynced?
    #[cfg(test)]
    // TEST-EXEMPT: test-only probe, exercised by the sync tests
    pub(crate) fn has_unsynced_writes(&self) -> bool {
        let unsynced = self.lock_unsynced();
        unsynced.days.iter().any(Option::is_some) || unsynced.dir || unsynced.parent
    }

    /// Syncs every DLQ day file written since the last call, and the DLQ
    /// directory entry when a day file (or the directory) was created.
    ///
    /// `append_record` never syncs: the frame drain's inline fallback can
    /// reach it, and it must not wait for the device. The seal writer task
    /// (after a cycle that escalated) and the escalation thread (after a
    /// batch, and at exit) call this instead, off the drain. The lock is held
    /// only to take the pending set, never across a sync. **O(days ≤ 2)**
    /// opens and syncs, plus at most two directory syncs.
    ///
    /// Returns `true` when there was nothing to sync or every sync succeeded.
    /// A failed item is put back so the next call retries it, counted on
    /// `tv_seal_dlq_sync_failed_total{reason="sync"}`, and logged once per
    /// failing episode. A day file removed since (replayed and cleared) has
    /// nothing left to lose and is skipped.
    pub fn sync_written(&self) -> bool {
        let pending = std::mem::take(&mut *self.lock_unsynced());
        if pending.days.iter().all(Option::is_none) && !pending.dir && !pending.parent {
            return true;
        }
        let mut failed = DlqUnsynced::default();
        let mut first_err: Option<std::io::Error> = None;
        for (day, now_unix_secs) in pending.days.into_iter().flatten() {
            let path = self.dlq_path(now_unix_secs);
            let outcome = match std::fs::OpenOptions::new().append(true).open(&path) {
                Ok(file) => file.sync_data(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            };
            if let Err(err) = outcome {
                failed.days[usize::from(failed.days[0].is_some())] = Some((day, now_unix_secs));
                first_err.get_or_insert(err);
            }
        }
        if pending.dir
            && let Err(err) = sync_directory(&self.dlq_dir)
        {
            failed.dir = true;
            first_err.get_or_insert(err);
        }
        if pending.parent
            && let Some(parent) = self.dlq_dir.parent()
            && let Err(err) = sync_directory(parent)
        {
            failed.parent = true;
            first_err.get_or_insert(err);
        }
        let Some(err) = first_err else {
            self.sync_failing.store(false, Ordering::Relaxed);
            return true;
        };
        metrics::counter!(SEAL_DLQ_SYNC_FAILED_COUNTER, "reason" => "sync").increment(1);
        {
            let mut unsynced = self.lock_unsynced();
            for (_, now_unix_secs) in failed.days.into_iter().flatten() {
                unsynced.note_day(now_unix_secs);
            }
            unsynced.dir |= failed.dir;
            unsynced.parent |= failed.parent;
        }
        if !self.sync_failing.swap(true, Ordering::Relaxed) {
            error!(
                code = ErrorCode::AggregatorSeal01IlpFailed.code_str(),
                dlq_dir = ?self.dlq_dir,
                ?err,
                "seal dead-letter: syncing the day file failed; records reported written may \
                 be lost on a power cut until a sync succeeds"
            );
        }
        false
    }

    /// Returns the path of the DLQ file for the given UTC unix
    /// timestamp (used to derive IST date). Pure helper.
    #[must_use]
    pub fn dlq_path(&self, now_unix_secs: i64) -> PathBuf {
        self.dlq_dir.join(ist_date_filename(now_unix_secs))
    }

    /// Append one record to the daily DLQ file as a single NDJSON
    /// line (`{...}\n`). Creates the DLQ directory + file if needed.
    /// O(1) per call; uses `BufWriter` to coalesce small writes.
    ///
    /// Per locked decision L-C1, this is the THIRD tier of the
    /// ring → spill → DLQ chain. If THIS append also fails, the
    /// writer task escalates to
    /// `error!(code = ErrorCode::AggregatorDrop01.code_str())` per
    /// the AGGREGATOR-DROP-01 runbook.
    pub fn append_record(&self, record: &SealDlqRecord, now_unix_secs: i64) -> Result<()> {
        let created_dir = !self.dlq_dir.is_dir();
        std::fs::create_dir_all(&self.dlq_dir)
            .with_context(|| format!("failed to create dlq dir {:?}", self.dlq_dir))?;
        let path = self.dlq_path(now_unix_secs);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open dlq file {path:?}"))?;
        // An empty file was (almost always) just created: its directory entry
        // must reach the device too. A false positive costs one extra
        // directory sync. Recorded before the write so a write that fails
        // after a partial line still gets that line synced.
        let created_file = file.metadata().map_or(true, |meta| meta.len() == 0);
        {
            let mut unsynced = self.lock_unsynced();
            unsynced.note_day(now_unix_secs);
            unsynced.dir |= created_file;
            unsynced.parent |= created_dir;
        }
        let mut writer = BufWriter::new(file);
        let line = serde_json::to_string(record)
            .with_context(|| "failed to serialise SealDlqRecord to JSON")?;
        writer
            .write_all(line.as_bytes())
            .with_context(|| format!("failed to write dlq line to {path:?}"))?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("failed to write dlq newline to {path:?}"))?;
        writer
            .flush()
            .with_context(|| format!("failed to flush dlq line to {path:?}"))?;
        Ok(())
    }

    /// Drains the daily DLQ file by reading every line into the
    /// returned `Vec`. Lines that fail to parse (corrupt / partial /
    /// truncated tail) are silently dropped with a `warn!` so a
    /// single bad line does NOT stall replay.
    ///
    /// After successful read the caller (writer task) deletes the
    /// DLQ file via [`Self::clear_dlq_for_date`].
    ///
    /// Returns an empty `Vec` if the DLQ file does not exist
    /// (the happy path on a fresh boot).
    pub fn read_all(&self, now_unix_secs: i64) -> Result<Vec<SealDlqRecord>> {
        let path = self.dlq_path(now_unix_secs);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open dlq file {path:?}"))?;
        let reader = BufReader::new(file);
        let mut all = Vec::new();
        let mut stale_refused = 0usize;
        for (line_no, line_result) in reader.lines().enumerate() {
            let line = match line_result {
                Ok(l) => l,
                Err(err) => {
                    warn!(
                        ?path,
                        line_no,
                        ?err,
                        "dlq line read failed — dropping and continuing"
                    );
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // Blank line — likely a trailing newline after the
                // last record. Skip silently (no warn).
                continue;
            }
            match serde_json::from_str::<SealDlqRecord>(trimmed) {
                // A range, not `<`: a record from a NEWER build is exactly as
                // unreadable as an older one — the case is a deploy rollback,
                // where this binary meets lines its successor wrote. Version 4
                // is still read (2026-10-01): 4 → 5 renumbered nothing.
                Ok(rec) if !seal_spill_version_is_readable(rec.format_version) => {
                    stale_refused += 1;
                }
                Ok(rec) => all.push(rec),
                Err(err) => {
                    warn!(
                        ?path,
                        line_no,
                        ?err,
                        "dlq line failed to parse — dropping and continuing"
                    );
                }
            }
        }
        if stale_refused > 0 {
            warn!(
                ?path,
                stale_refused,
                current_format_version = SEAL_SPILL_FORMAT_VERSION,
                "refused dlq lines written under a DIFFERENT seal format version \
                 (their tf_ordinal belongs to another timeframe numbering, so \
                 decoding them would file a bar under the wrong timeframe)"
            );
        }
        info!(?path, count = all.len(), "drained dlq file");
        Ok(all)
    }

    /// Removes the DLQ file for the given date. Called by the
    /// writer task after `read_all` is fully replayed via the
    /// binary path. Idempotent: missing file returns Ok.
    pub fn clear_dlq_for_date(&self, now_unix_secs: i64) -> Result<()> {
        let path = self.dlq_path(now_unix_secs);
        if !path.exists() {
            return Ok(());
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove dlq file {path:?}"))?;
        info!(?path, "dlq file cleared after successful drain");
        Ok(())
    }
}

impl Default for SealDlqWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Total bytes currently held in the DLQ directory.
///
/// # Why the DLQ is MEASURED and never pruned (2026-08-19)
///
/// The 2026-08-19 disk audit flagged `data/dlq/` alongside `data/spill/` as
/// having no size cap. The spill directory got a retention sweep. This one
/// deliberately does NOT, and the asymmetry is the point.
///
/// Spill holds seals awaiting replay — transient by design, and an aged file
/// there means the replay path broke. The DLQ holds the last-resort record of
/// seals that were LOST, in operator-readable NDJSON precisely so that after
/// an incident someone can `cat | jq` and see exactly what went missing.
/// Deleting it to reclaim disk would destroy the evidence it exists to
/// preserve — the one directory on the box where automatic deletion is the
/// wrong default.
///
/// Its growth is also self-limiting in a way spill's is not: nothing is
/// written here in normal operation, so a growing DLQ IS the incident signal.
/// Measuring it makes that signal observable; pruning it would erase it.
///
/// Exported as `tv_seal_dlq_bytes` so a rising DLQ is visible on the
/// dashboard rather than discovered when the volume fills.
#[must_use]
pub fn dlq_bytes_at(dlq_dir: &Path) -> u64 {
    // O(1) EXEMPT: periodic cold measurement, never the per-seal append
    let Ok(entries) = std::fs::read_dir(dlq_dir) else {
        return 0; // missing dir — nothing written yet, the healthy case
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("ndjson"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Measures the DLQ and publishes `tv_seal_dlq_bytes`. Cold path — called
/// from the periodic retention loop alongside the spill sweep.
// TEST-EXEMPT: thin gauge wrapper — byte accounting is covered by test_dlq_bytes_at_counts_only_ndjson_and_tolerates_a_missing_dir; this layer only publishes the metric.
pub fn record_dlq_bytes(dlq_dir: &Path) -> u64 {
    let bytes = dlq_bytes_at(dlq_dir);
    metrics::gauge!("tv_seal_dlq_bytes").set(bytes as f64);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_serialized_seal(sid: u64, seg: u8, tf: u8, bucket: u32, close: f64) -> SerializedSeal {
        SerializedSeal {
            security_id: sid,
            exchange_segment_code: seg,
            feed: Feed::Dhan,
            tf_ordinal: tf,
            bucket_start_ist_secs: bucket,
            tick_count: 5,
            volume: 1234,
            bucket_start_cumulative: 1000,
            oi: 50_000,
            open: 100.0,
            high: 105.0,
            low: 99.0,
            close,
            close_pct_from_prev_day: 1.5,
            bucket_open_prev_close: 98.5,
            total_buy_qty: 89_600,
            total_sell_qty: 4_800,
            open_pct: 7.7,
            change_pct: 1.5,
            open_gap_pct: 0.8,
        }
    }

    fn temp_dlq_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-dlq-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    #[test]
    fn test_seal_dlq_record_default_is_zeroed() {
        let r = SealDlqRecord::default();
        assert_eq!(r.security_id, 0);
        assert_eq!(r.exchange_segment_code, 0);
        assert_eq!(r.tf_ordinal, 0);
        assert_eq!(r.bucket_start_ist_secs, 0);
        assert_eq!(r.tick_count, 0);
        assert_eq!(r.volume, 0);
        assert_eq!(r.bucket_start_cumulative, 0);
        assert_eq!(r.oi, 0);
        assert_eq!(r.open, 0.0);
        assert_eq!(r.high, 0.0);
        assert_eq!(r.low, 0.0);
        assert_eq!(r.close, 0.0);
        assert_eq!(r.close_pct_from_prev_day, 0.0);
        assert_eq!(r.bucket_open_prev_close, 0.0);
        assert_eq!(r.total_buy_qty, 0);
        assert_eq!(r.total_sell_qty, 0);
    }

    /// A DLQ line from EITHER earlier era still parses — the deploy-boundary
    /// case, now in both directions.
    ///
    /// `data/dlq/seals-*.ndjson` is append-only and survives a restart, so the
    /// first boot after this change reads whatever the previous builds wrote.
    /// Two distinct shapes exist on disk:
    ///
    ///   * pre-2026-09-10 — carries `bucket_open_prev_close`, which is the key
    ///     this build writes again, so it decodes with its baseline intact.
    ///   * 2026-09-10..2026-09-18 — carries `net_volume_signed` /
    ///     `net_volume_classified` and NO baseline. Those keys no longer exist
    ///     on the record, so the parse depends on them being IGNORED (no
    ///     `deny_unknown_fields`) rather than rejected, and the missing
    ///     baseline must default to `0.0` rather than failing.
    ///
    /// `0.0` is the honest reading for that second shape: the record genuinely
    /// did not carry a baseline, and `signed_volume()` reports such a bar
    /// positive rather than inventing a comparison.
    #[test]
    fn a_dlq_line_from_either_earlier_era_parses_rather_than_failing() {
        let with_baseline = r#"{
            "security_id": 13,
            "exchange_segment_code": 0,
            "feed": "dhan",
            "tf_ordinal": 0,
            "bucket_start_ist_secs": 1716000900,
            "tick_count": 5,
            "volume": 1234,
            "bucket_start_cumulative": 1000,
            "oi": 50000,
            "open": 100.0,
            "high": 105.0,
            "low": 99.0,
            "close": 102.5,
            "close_pct_from_prev_day": 1.5,
            "bucket_open_prev_close": 24200.10,
            "total_buy_qty": 89600,
            "total_sell_qty": 4800,
            "open_pct": 0.0,
            "change_pct": 0.0,
            "open_gap_pct": 0.0
        }"#;

        let r: SealDlqRecord = serde_json::from_str(with_baseline)
            .expect("a pre-2026-09-10 DLQ line must still parse");
        assert_eq!(r.security_id, 13, "the rest of the record survives intact");
        assert_eq!(r.volume, 1234);
        assert_eq!(
            r.bucket_open_prev_close, 24200.10,
            "the key means what it meant before, so the baseline survives"
        );

        // The flow-era shape: the two retired keys present, the baseline absent.
        let flow_era = r#"{
            "security_id": 13,
            "exchange_segment_code": 0,
            "feed": "dhan",
            "tf_ordinal": 0,
            "bucket_start_ist_secs": 1716000900,
            "tick_count": 5,
            "volume": 1234,
            "bucket_start_cumulative": 1000,
            "oi": 50000,
            "open": 100.0,
            "high": 105.0,
            "low": 99.0,
            "close": 102.5,
            "close_pct_from_prev_day": 1.5,
            "net_volume_signed": -4242,
            "net_volume_classified": true,
            "total_buy_qty": 89600,
            "total_sell_qty": 4800,
            "open_pct": 0.0,
            "change_pct": 0.0,
            "open_gap_pct": 0.0
        }"#;

        let r: SealDlqRecord = serde_json::from_str(flow_era)
            .expect("a 2026-09-10..09-18 DLQ line must parse, with the retired keys ignored");
        assert_eq!(r.security_id, 13, "the rest of the record survives intact");
        assert_eq!(r.volume, 1234);
        assert_eq!(
            r.bucket_open_prev_close, 0.0,
            "no baseline was recorded, so none is invented"
        );

        // And the replay path it feeds signs the bar positive rather than
        // comparing its close against a fabricated baseline.
        let seal = SerializedSeal::from(&r);
        let replayed = seal
            .try_into_buffered_seal()
            .expect("a known tf ordinal round-trips");
        assert_eq!(
            replayed.state.signed_volume(),
            1234,
            "no baseline means positive, never a sign derived from 0.0"
        );
    }

    #[test]
    fn test_seal_dlq_record_serde_roundtrip_preserves_every_field() {
        let original = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 102.5));
        let json = serde_json::to_string(&original).expect("serialise");
        let decoded: SealDlqRecord = serde_json::from_str(&json).expect("parse");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_seal_dlq_record_json_field_names_are_stable_for_jq() {
        // Operator demand: `cat data/dlq/seals_v4-*.ndjson | jq` must
        // work. Pin the exact JSON keys so a future serde rename
        // does not silently break operator tooling.
        let r = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 102.5));
        let json = serde_json::to_string(&r).expect("serialise");
        for field in [
            "security_id",
            "exchange_segment_code",
            "tf_ordinal",
            "bucket_start_ist_secs",
            "tick_count",
            "volume",
            "bucket_start_cumulative",
            "oi",
            "open",
            "high",
            "low",
            "close",
            "close_pct_from_prev_day",
            "bucket_open_prev_close",
            "total_buy_qty",
            "total_sell_qty",
            "open_pct",
            "change_pct",
            "open_gap_pct",
        ] {
            assert!(
                json.contains(&format!("\"{field}\"")),
                "JSON missing expected key {field}: {json}"
            );
        }
    }

    #[test]
    fn test_from_serialized_seal_to_dlq_record_lossless() {
        let s = mk_serialized_seal(13, 0, 0, 1_716_000_900, 102.5);
        let r = SealDlqRecord::from(&s);
        assert_eq!(r.security_id, s.security_id);
        assert_eq!(r.exchange_segment_code, s.exchange_segment_code);
        assert_eq!(r.tf_ordinal, s.tf_ordinal);
        assert_eq!(r.bucket_start_ist_secs, s.bucket_start_ist_secs);
        assert_eq!(r.tick_count, s.tick_count);
        assert_eq!(r.volume, s.volume);
        assert_eq!(r.bucket_start_cumulative, s.bucket_start_cumulative);
        assert_eq!(r.oi, s.oi);
        assert_eq!(r.open, s.open);
        assert_eq!(r.high, s.high);
        assert_eq!(r.low, s.low);
        assert_eq!(r.close, s.close);
        assert_eq!(r.close_pct_from_prev_day, s.close_pct_from_prev_day);
        assert_eq!(r.bucket_open_prev_close, s.bucket_open_prev_close);
        assert_eq!(r.total_buy_qty, s.total_buy_qty);
        assert_eq!(r.total_sell_qty, s.total_sell_qty);
    }

    #[test]
    fn test_from_dlq_record_to_serialized_seal_lossless() {
        let original = mk_serialized_seal(25, 1, 4, 1_716_001_500, 200.75);
        let r = SealDlqRecord::from(&original);
        let recovered = SerializedSeal::from(&r);
        assert_eq!(recovered, original);
    }

    #[test]
    fn test_dlq_record_handles_negative_oi_and_pct() {
        // i64 OI can be negative for short positions; pct fields can
        // be negative on red days — JSON round-trip MUST preserve.
        let s = SerializedSeal {
            security_id: 25,
            exchange_segment_code: 1,
            feed: Feed::Dhan,
            tf_ordinal: 4,
            bucket_start_ist_secs: 1_716_001_500,
            tick_count: 0,
            volume: 0,
            bucket_start_cumulative: 0,
            oi: -42_000,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            close_pct_from_prev_day: -3.5,
            bucket_open_prev_close: 98.5,
            total_buy_qty: 0,
            total_sell_qty: 0,
            open_pct: -50.0,
            change_pct: -3.5,
            open_gap_pct: -1.2,
        };
        let r = SealDlqRecord::from(&s);
        let json = serde_json::to_string(&r).expect("serialise");
        let decoded: SealDlqRecord = serde_json::from_str(&json).expect("parse");
        let recovered = SerializedSeal::from(&decoded);
        assert_eq!(recovered, s);
    }

    #[test]
    fn test_ist_date_filename_uses_ndjson_suffix() {
        let utc_noon = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let name = ist_date_filename(utc_noon);
        assert_eq!(name, "seals_v4-2026-01-01.ndjson");
        assert!(name.starts_with(crate::seal_writer_task::SEAL_FILE_PREFIX));
        assert!(
            !name.starts_with(crate::seal_writer_task::LEGACY_SEAL_FILE_PREFIX),
            "a v4 DLQ file must be invisible to a pre-v4 binary's drain"
        );
        assert!(name.ends_with(".ndjson"));
    }

    #[test]
    fn test_ist_date_filename_crosses_to_next_day_at_ist_midnight() {
        // 2026-05-09 18:30:00 UTC = 2026-05-10 00:00:00 IST.
        let utc = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        let name = ist_date_filename(utc);
        assert_eq!(name, "seals_v4-2026-05-10.ndjson");
    }

    #[test]
    fn test_seal_dlq_writer_new_uses_production_dir() {
        let writer = SealDlqWriter::new();
        assert_eq!(writer.dlq_dir, PathBuf::from(SEAL_DLQ_DIR));
    }

    #[test]
    fn test_seal_dlq_writer_default_matches_new() {
        let a = SealDlqWriter::default();
        let b = SealDlqWriter::new();
        assert_eq!(a.dlq_dir, b.dlq_dir);
    }

    #[test]
    fn test_seal_dlq_writer_dlq_path_uses_ist_date_in_filename() {
        let dir = temp_dlq_dir("path-ist-date");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let utc_noon = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let p = writer.dlq_path(utc_noon);
        assert!(p.to_string_lossy().ends_with("seals_v4-2026-05-10.ndjson"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_append_record_then_read_all_roundtrip() {
        let dir = temp_dlq_dir("append-then-read");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let r2 = SealDlqRecord::from(&mk_serialized_seal(25, 0, 4, 1_716_001_500, 200.0));
        writer.append_record(&r1, now).expect("append r1");
        writer.append_record(&r2, now).expect("append r2");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], r1);
        assert_eq!(drained[1], r2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_sync_written_syncs_each_written_day_and_its_new_directory_entries() {
        // 2026-10-04: the DLQ was flushed to the page cache and never synced.
        // `append_record` records what to sync (it never syncs: the drain's
        // inline fallback reaches it); `sync_written` syncs it and clears it.
        let root = temp_dlq_dir("sync-written");
        let dir = root.join("dlq");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        assert!(writer.sync_written(), "nothing written is a clean no-op");
        let day_one = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("valid"))
            .timestamp();
        let day_two = day_one + 86_400;
        let record = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        writer
            .append_record(&record, day_one)
            .unwrap_or_else(|err| panic!("append day one: {err}"));
        writer
            .append_record(&record, day_two)
            .unwrap_or_else(|err| panic!("append day two: {err}"));
        {
            let unsynced = writer.lock_unsynced();
            assert_eq!(
                unsynced.days.iter().flatten().count(),
                2,
                "both days must be pending"
            );
            assert!(unsynced.dir, "a new day file's entry must be pending");
            assert!(unsynced.parent, "the created directory's entry too");
        }
        assert!(writer.sync_written(), "both days must sync");
        assert!(!writer.has_unsynced_writes(), "a clean sync clears the set");
        // A second record on an existing, non-empty file needs no directory
        // sync, only the file.
        writer
            .append_record(&record, day_two)
            .unwrap_or_else(|err| panic!("append again: {err}"));
        {
            let unsynced = writer.lock_unsynced();
            assert!(!unsynced.dir && !unsynced.parent, "no new entry was made");
            assert_eq!(unsynced.days.iter().flatten().count(), 1);
        }
        // A day file replayed and removed in between has nothing to lose.
        writer
            .clear_dlq_for_date(day_two)
            .unwrap_or_else(|err| panic!("clear: {err}"));
        assert!(writer.sync_written(), "a removed day file is skipped");
        assert!(!writer.has_unsynced_writes());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_note_day_keeps_the_two_newest_days_when_a_third_arrives_unsynced() {
        let mut unsynced = DlqUnsynced::default();
        unsynced.note_day(0);
        unsynced.note_day(86_400);
        unsynced.note_day(86_400 + 60);
        assert_eq!(
            unsynced.days.iter().flatten().count(),
            2,
            "same day is one slot"
        );
        unsynced.note_day(2 * 86_400);
        let mut days: Vec<i64> = unsynced.days.iter().flatten().map(|(d, _)| *d).collect();
        days.sort_unstable();
        assert_eq!(
            days,
            vec![ist_day_number(86_400), ist_day_number(2 * 86_400)],
            "the oldest day is the one given up"
        );
    }

    #[test]
    fn test_append_record_never_syncs_and_sync_written_holds_no_lock() {
        // `append_record` is reachable from the frame drain's inline fallback,
        // so it must never wait for the device; `sync_written` must release
        // the lock before it syncs, or an inline append would wait for it.
        let src = include_str!("seal_dlq.rs");
        let body = |name: &str| {
            let start = src
                .find(name)
                .unwrap_or_else(|| panic!("{name} must exist"));
            let rest = &src[start..];
            let end = rest.find("\n    ///").unwrap_or(rest.len());
            &rest[..end]
        };
        let append = body("pub fn append_record(");
        assert!(
            !append.contains("sync_data") && !append.contains("sync_all"),
            "append_record must never sync"
        );
        let sync = body("pub fn sync_written(");
        let taken = sync
            .find("std::mem::take")
            .unwrap_or_else(|| panic!("the pending set must be taken"));
        // The guard is a temporary of the `take` statement, so it is dropped
        // at that statement's end, before any sync.
        assert!(
            sync[taken..].starts_with("std::mem::take(&mut *self.lock_unsynced());"),
            "the lock must live only for the take"
        );
        assert!(!sync[..taken].contains("sync_data"));
        let first_sync = sync
            .find("sync_data")
            .unwrap_or_else(|| panic!("the day files must be synced"));
        let after_take = taken + "std::mem::take(&mut *self.lock_unsynced());".len();
        assert!(
            sync[after_take..first_sync]
                .matches("lock_unsynced")
                .count()
                == 0,
            "no lock may be held across a sync"
        );
    }

    #[test]
    fn test_seal_dlq_writer_read_all_on_missing_file_returns_empty() {
        let dir = temp_dlq_dir("missing-file");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc::now().timestamp();
        let drained = writer.read_all(now).expect("ok");
        assert!(drained.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_dlq_for_date_removes_file() {
        let dir = temp_dlq_dir("clear-removes");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        writer.append_record(&r1, now).expect("append");
        let path = writer.dlq_path(now);
        assert!(path.exists());
        writer.clear_dlq_for_date(now).expect("clear");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_dlq_for_date_on_missing_file_is_noop() {
        let dir = temp_dlq_dir("clear-missing");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc::now().timestamp();
        // No file written. Clear must succeed.
        writer.clear_dlq_for_date(now).expect("idempotent");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_skips_corrupt_lines_gracefully() {
        // Manually write 3 lines: valid, garbage, valid. Reader must
        // return both valid records and silently drop the corrupt
        // line with a warn — no panic, no early return.
        let dir = temp_dlq_dir("corrupt-line");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let r2 = SealDlqRecord::from(&mk_serialized_seal(25, 0, 4, 1_716_001_500, 200.0));
        writer.append_record(&r1, now).expect("append r1");
        // Manually inject a corrupt line.
        let path = writer.dlq_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open append");
            f.write_all(b"this-is-not-json\n").expect("write garbage");
            f.flush().expect("flush");
        }
        writer.append_record(&r2, now).expect("append r2");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2, "corrupt line must NOT stall replay");
        assert_eq!(drained[0], r1);
        assert_eq!(drained[1], r2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_skips_blank_lines_silently() {
        // A trailing newline produces a blank line on read — must be
        // skipped silently (no warn flood on the happy path).
        let dir = temp_dlq_dir("blank-line");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        writer.append_record(&r1, now).expect("append");
        // Append blank lines manually.
        let path = writer.dlq_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open append");
            f.write_all(b"\n\n   \n").expect("write blanks");
            f.flush().expect("flush");
        }
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0], r1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_handles_50_records_in_fifo_order() {
        let dir = temp_dlq_dir("fifo-50");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let n: u32 = 50;
        for i in 0..n {
            let r = SealDlqRecord::from(&mk_serialized_seal(
                13,
                0,
                (i % 9) as u8,
                1_716_000_000 + i,
                100.0 + i as f64,
            ));
            writer.append_record(&r, now).expect("append");
        }
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), n as usize);
        for (i, r) in drained.iter().enumerate() {
            assert_eq!(r.bucket_start_ist_secs, 1_716_000_000 + i as u32);
            assert_eq!(r.close, 100.0 + i as f64);
            assert_eq!(r.tf_ordinal, (i % 9) as u8);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_distinguishes_segments_for_i_p1_11() {
        // I-P1-11: composite key (security_id, exchange_segment_code).
        // Same security_id with different exchange_segment_code MUST
        // round-trip as two distinct records — no collapse.
        let dir = temp_dlq_dir("i-p1-11");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let seg0 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let seg1 = SealDlqRecord::from(&mk_serialized_seal(13, 1, 0, 1_716_000_900, 200.0));
        writer.append_record(&seg0, now).expect("append seg0");
        writer.append_record(&seg1, now).expect("append seg1");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].exchange_segment_code, 0);
        assert_eq!(drained[1].exchange_segment_code, 1);
        assert_eq!(drained[0].close, 100.0);
        assert_eq!(drained[1].close, 200.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_appends_one_line_per_record() {
        // Wire-format invariant: NDJSON = one JSON object per line.
        // Operator's `cat | jq` and `wc -l` MUST produce the record
        // count.
        let dir = temp_dlq_dir("one-line-per-record");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let r2 = SealDlqRecord::from(&mk_serialized_seal(25, 0, 4, 1_716_001_500, 200.0));
        writer.append_record(&r1, now).expect("append r1");
        writer.append_record(&r2, now).expect("append r2");
        let path = writer.dlq_path(now);
        let raw = std::fs::read_to_string(&path).expect("read raw");
        // Two records => exactly two newlines (one per record).
        assert_eq!(raw.matches('\n').count(), 2);
        // No JSON object spans a line.
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let _: SealDlqRecord = serde_json::from_str(line).expect("each line parses");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
    // ---- DLQ byte measurement (2026-08-19) --------------------------------

    #[test]
    fn test_dlq_bytes_at_counts_only_ndjson_and_tolerates_a_missing_dir() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("tv-dlq-bytes-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);

        // A directory that was never created is the HEALTHY case — nothing
        // has ever been dead-lettered — and must read 0, not fail.
        assert_eq!(dlq_bytes_at(&dir), 0, "missing dir is the healthy zero");

        std::fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(dlq_bytes_at(&dir), 0, "empty dir is zero");

        std::fs::write(dir.join("seals-2026-08-19.ndjson"), vec![0_u8; 700]).expect("w1");
        std::fs::write(dir.join("seals-2026-08-18.ndjson"), vec![0_u8; 300]).expect("w2");
        // A foreign file must not inflate the DLQ signal — a false "the DLQ
        // is growing" reading would send an operator hunting a non-incident.
        std::fs::write(dir.join("notes.txt"), vec![0_u8; 999_999]).expect("w3");

        assert_eq!(dlq_bytes_at(&dir), 1000, "only .ndjson records count");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
