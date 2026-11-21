//! The 20 Nov quarantine copy (daily-universe Quote 63, 2026-11-20; plan
//! ITEM UTC-REPAIR): a verified, create-only copy of each wrong 2026-11-20
//! partition (and each local seal/spill/DLQ file of that day) to
//! `s3://<cold>/quarantine/2026-11-20-ltt-utc-shift/`, and, only after that
//! copy is verified and its `day_repair_audit` row is ACKED, the partition's
//! drop through the archiver's own last-moment recount
//! ([`super::snapshot_gate::recheck_and_drop`]).
//!
//! A child module of `partition_archive` so it reuses the archiver's private
//! steps unchanged: the ordered sliced export
//! ([`super::PartitionArchiver::export_ordered_csv`]), the recount
//! ([`super::PartitionArchiver::recount_rows`]), the type-state verify
//! ([`super::verify_archive`]) and the proof-gated drop
//! ([`super::PartitionArchiver::drop_partition`]).
//!
//! ## What it never does (Quote 63 REJECT rows)
//!
//! * Never deletes, overwrites or expires an S3 object: placement is
//!   HeadObject then a conditional create (`If-None-Match: *`); an identical
//!   object is reused, different bytes go to a content-addressed `.sha-<hex>`
//!   key beside it; [`QuarantineIo::delete_own_upload`] returns an error and
//!   never calls S3, and every copy is passed to the drop as not uploaded by
//!   this run, so the recount-mismatch arm never even asks.
//! * Never touches a canonical key (`questdb-partitions/...`), a manifest, an
//!   amend record or a `partition_archive_audit` row: it never calls
//!   `place_archive`, `daily_export::note_archived_copy`, `record_failure` or
//!   `append_audit`. Its own record is `day_repair_audit`.
//! * Never another date, never `market_depth*`, never a cross-verify, SEBI or
//!   audit table: [`quarantine_partition_allowed`] admits only the held kinds
//!   (`ticks`, `tick_quotes`, `feed_aux_packets`, `candles_*`) and only
//!   partitions of 2026-11-20.
//! * Never drops a partition whose copy is not verified (rows equal to the
//!   recount, stored length equal to the gzip, stored SHA-256 equal to the
//!   export's) or whose WAL is not provably applied.
//!
//! ## Cost
//!
//! Offline, once, with the app stopped: O(rows) per partition for the export
//! (QuestDB sorts each 10-minute slice by the DEDUP key), O(bytes) for the
//! gzip, the hash and the upload, a handful of HTTP round trips per object.
//! Nothing here is on a hot path.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tracing::{error, info};

use tickvault_common::error_code::ErrorCode;

use super::snapshot_gate::{
    ArchiveIo, CopiedPartition, HourFrameSpan, InFlightVerdict, recheck_and_drop,
};
use super::{
    ArchiveOutcome, ArchiveRunSummary, PartitionArchiver, WalApplyVerdict, daily_export,
    partition_range_bounds, remove_temp_file, verify_archive, wal_apply_verdict,
};
use crate::day_repair_audit_persistence::{
    DayRepairAuditRow, DayRepairAuditWriter, DayRepairOutcome, DayRepairStep,
};
use crate::s3_cold::{ExistingObject, S3ObjectMeta, base64_encode, classify_existing, hex_encode};
use crate::wal_suspension_watcher::WalTableRow;

/// The one day this repair touches (daily-universe Quote 63).
pub const QUARANTINE_REPAIR_DAY: &str = "2026-11-20";

/// The S3 prefix every quarantine copy lives under (Quote 63). Never the
/// archive's canonical prefix.
pub const QUARANTINE_PREFIX: &str = "quarantine/2026-11-20-ltt-utc-shift";

/// Longest local file name (relative path) a file copy accepts, in bytes.
pub const QUARANTINE_FILE_NAME_MAX_BYTES: usize = 512;

/// Read chunk for hashing a local file before its upload.
const FILE_HASH_CHUNK_BYTES: usize = 1 << 20;

/// Whether `table`'s `partition` may be copied (and dropped) by this repair:
/// a held kind ([`crate::held_days::table_is_held_kind`]), never a depth
/// table, and a DAY or HOUR partition of [`QUARANTINE_REPAIR_DAY`]. Pure,
/// O(1).
#[must_use]
pub fn quarantine_partition_allowed(table: &str, partition: &str) -> bool {
    crate::held_days::table_is_held_kind(table)
        && !table.starts_with("market_depth")
        && crate::held_days::partition_day(partition).is_some()
        && partition.get(..10) == Some(QUARANTINE_REPAIR_DAY)
        && crate::partition_manager::is_valid_partition_name(partition)
}

/// The tables this repair copies: every export table of a held kind (the
/// three hour tables and every emitted candle table). O(tables).
#[must_use]
pub fn quarantine_tables() -> Vec<&'static str> {
    daily_export::export_tables()
        .into_iter()
        .filter(|t| crate::held_days::table_is_held_kind(t) && !t.starts_with("market_depth"))
        .collect()
}

/// `{prefix}/partitions/{table}/{partition}.csv.gz`. Pure.
#[must_use]
pub fn quarantine_partition_key(table: &str, partition: &str) -> String {
    format!("{QUARANTINE_PREFIX}/partitions/{table}/{partition}.csv.gz")
}

/// The content-addressed key beside [`quarantine_partition_key`], used when
/// that key already holds different bytes. Pure.
#[must_use]
pub fn quarantine_partition_sidecar_key(table: &str, partition: &str, sha256_hex: &str) -> String {
    format!("{QUARANTINE_PREFIX}/partitions/{table}/{partition}.sha-{sha256_hex}.csv.gz")
}

/// `{prefix}/files/{name}`. Pure.
#[must_use]
pub fn quarantine_file_key(name: &str) -> String {
    format!("{QUARANTINE_PREFIX}/files/{name}")
}

/// The content-addressed key beside [`quarantine_file_key`]. Pure.
#[must_use]
pub fn quarantine_file_sidecar_key(name: &str, sha256_hex: &str) -> String {
    format!("{QUARANTINE_PREFIX}/files/{name}.sha-{sha256_hex}")
}

/// Whether `name` is a safe relative file name for [`quarantine_file_key`]:
/// non-empty, at most [`QUARANTINE_FILE_NAME_MAX_BYTES`], ASCII letters,
/// digits, `.`, `_`, `-` and `/` only, no leading `/`, no empty, `.` or `..`
/// segment. Pure, O(name).
#[must_use]
pub fn quarantine_file_name_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= QUARANTINE_FILE_NAME_MAX_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && name
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// How far [`PartitionArchiver::quarantine_partition`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineMode {
    /// Copy and verify only (the dry run's and the copy step's mode).
    CopyOnly,
    /// Copy, verify, record, then drop through the last-moment recount.
    CopyAndDrop,
}

/// One verified object in the quarantine prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedObject {
    /// `table/partition` or `files/<name>`.
    pub object: String,
    /// Where it lives in the cold bucket.
    pub s3_key: String,
    /// Rows copied (a partition); `None` for a file.
    pub rows: Option<u64>,
    /// Bytes stored in S3 (the gzip for a partition, the raw bytes for a file).
    pub bytes: u64,
    /// SHA-256 (hex) of the stored bytes, equal to S3's stored checksum.
    pub sha256_hex: String,
    /// `true` when an identical object was already there (a re-run).
    pub reused: bool,
}

/// How one quarantine call ended. Only `Dropped` means a partition left
/// QuestDB; every other arm left QuestDB and S3 as they were apart from a
/// verified create-only copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineOutcome {
    /// Refused before any work (outside the allowlist, a bad name).
    Refused {
        /// Stable reason label.
        reason: &'static str,
    },
    /// Something did not prove out; nothing destructive happened.
    Kept {
        /// Stable stage label.
        stage: &'static str,
        /// What went wrong.
        detail: String,
    },
    /// Copied and verified (and recorded); nothing dropped.
    Copied(QuarantinedObject),
    /// Copied, verified, recorded, then dropped.
    Dropped(QuarantinedObject),
}

/// The next placement action for one key's HeadObject classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementStep {
    /// Nothing there: conditional create.
    Upload,
    /// The same bytes are there: reuse.
    Reuse,
    /// Different bytes at the primary key: try the content-addressed key.
    TrySidecar,
    /// Different bytes at the content-addressed key: refuse.
    Conflict,
}

/// Pure placement decision for one key. O(1).
const fn placement_step(existing: ExistingObject, at_sidecar: bool) -> PlacementStep {
    match (existing, at_sidecar) {
        (ExistingObject::Absent, _) => PlacementStep::Upload,
        (ExistingObject::Identical, _) => PlacementStep::Reuse,
        (ExistingObject::Different, false) => PlacementStep::TrySidecar,
        (ExistingObject::Different, true) => PlacementStep::Conflict,
    }
}

/// Whether S3's stored object at a key is exactly the local bytes (length
/// AND stored SHA-256). Pure, O(1).
fn stored_matches(meta: &S3ObjectMeta, len: u64, sha256_b64: &str) -> bool {
    meta.len == len && meta.checksum_sha256_b64.as_deref() == Some(sha256_b64)
}

/// SHA-256 and length of a local file, read in chunks. O(file bytes).
fn file_sha256_and_len(path: &Path) -> std::io::Result<([u8; 32], u64)> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; FILE_HASH_CHUNK_BYTES];
    let mut len: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len = len.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok((hasher.finalize().into(), len))
}

/// The archiver I/O behind the quarantine drop. `recheck_and_drop` drives it
/// exactly as it drives the archiver, with every S3 delete, drop-tally write
/// and `partition_archive_audit` row replaced by nothing or by a
/// `day_repair_audit` row.
pub(super) struct QuarantineIo<'a> {
    archiver: &'a PartitionArchiver,
    audit: &'a mut DayRepairAuditWriter,
    run_id: &'a str,
    /// The verified copy of the current partition.
    copy: Option<QuarantinedObject>,
    /// Why the current partition was kept, when it was.
    kept: Option<(&'static str, String)>,
    /// Whether the DROP was accepted.
    dropped: bool,
}

impl<'a> QuarantineIo<'a> {
    pub(super) fn new(
        archiver: &'a PartitionArchiver,
        audit: &'a mut DayRepairAuditWriter,
        run_id: &'a str,
    ) -> Self {
        Self {
            archiver,
            audit,
            run_id,
            copy: None,
            kept: None,
            dropped: false,
        }
    }

    /// Writes one audit row and waits for its ACK. A failure is logged with
    /// a code; the caller decides whether it gates anything.
    fn record(&mut self, row: &DayRepairAuditRow) -> Result<()> {
        let written = self.audit.append_and_flush(row);
        if let Err(ref err) = written {
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                step = row.step.as_str(),
                object = %row.object,
                outcome = row.outcome.as_str(),
                error = %format!("{err:#}"),
                "day repair: the day_repair_audit row was not acknowledged"
            );
        }
        written
    }

    /// Records why `object` is kept (coded error and a best-effort audit
    /// row) and remembers it. Nothing destructive follows a keep.
    fn keep(&mut self, step: DayRepairStep, object: &str, stage: &'static str, detail: String) {
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            step = step.as_str(),
            object,
            stage,
            detail = %detail,
            "day repair: quarantine step did not prove out — object KEPT, nothing dropped \
             or deleted; re-run the step once the cause is fixed"
        );
        let mut row = DayRepairAuditRow::now(
            QUARANTINE_REPAIR_DAY,
            step,
            object,
            DayRepairOutcome::Kept,
            self.run_id,
        );
        row.detail = format!("{stage}: {detail}");
        // Best effort: the keep is already the safe outcome.
        let _recorded = self.record(&row);
        self.kept = Some((stage, detail));
    }

    /// The kept outcome of the current partition.
    fn kept_outcome(&mut self, fallback_stage: &'static str) -> QuarantineOutcome {
        let (stage, detail) = self
            .kept
            .take()
            .unwrap_or_else(|| (fallback_stage, "no keep reason recorded".to_string()));
        QuarantineOutcome::Kept { stage, detail }
    }

    /// `true` only when `table`'s WAL is provably applied
    /// (`writerTxn == sequencerTxn`, not suspended); otherwise keeps.
    async fn wal_applied(&mut self, step: DayRepairStep, table: &str, object: &str) -> bool {
        let verdict = match self.archiver.fetch_wal_tables().await {
            Ok(rows) => wal_apply_verdict(&rows, table),
            Err(err) => {
                self.keep(step, object, "wal_probe_failed", format!("{err:#}"));
                return false;
            }
        };
        if verdict == WalApplyVerdict::Applied {
            return true;
        }
        self.keep(
            step,
            object,
            "wal_not_applied",
            format!("wal_tables() reads {}", verdict.as_str()),
        );
        false
    }

    /// Places `path` (`len` bytes, SHA-256 `sha256_b64`) at `primary`, or at
    /// `sidecar` when `primary` holds different bytes. Returns the key used
    /// and whether it was reused. A failed HeadObject never reads as absent.
    async fn place(
        &mut self,
        path: &Path,
        primary: &str,
        sidecar: &str,
        len: u64,
        sha256_b64: &str,
    ) -> std::result::Result<(String, bool), (&'static str, String)> {
        let mut key = primary;
        let mut at_sidecar = false;
        loop {
            let meta = self
                .archiver
                .cold
                .head(key)
                .await
                .map_err(|err| ("head_failed", format!("HeadObject {key}: {err:#}")))?;
            match placement_step(
                classify_existing(meta.as_ref(), len, sha256_b64),
                at_sidecar,
            ) {
                PlacementStep::Upload => {
                    self.archiver
                        .cold
                        .put_if_absent_path(path, key, sha256_b64)
                        .await
                        .map_err(|err| ("upload_failed", format!("PutObject {key}: {err:#}")))?;
                    return Ok((key.to_string(), false));
                }
                PlacementStep::Reuse => return Ok((key.to_string(), true)),
                PlacementStep::TrySidecar => {
                    key = sidecar;
                    at_sidecar = true;
                }
                PlacementStep::Conflict => {
                    return Err((
                        "s3_conflict",
                        format!(
                            "{primary} and its content-addressed key {sidecar} both hold \
                             different bytes; neither is touched"
                        ),
                    ));
                }
            }
        }
    }

    /// Reads back the stored object at `key`: `Ok(Some(len))` only when it
    /// holds exactly `len` bytes with SHA-256 `sha256_b64`.
    async fn stored_len(
        &self,
        key: &str,
        len: u64,
        sha256_b64: &str,
    ) -> std::result::Result<Option<u64>, (&'static str, String)> {
        match self.archiver.cold.head(key).await {
            Ok(Some(meta)) if stored_matches(&meta, len, sha256_b64) => Ok(Some(meta.len)),
            Ok(Some(meta)) => Err((
                "s3_mismatch",
                format!(
                    "stored {key} is {} bytes sha256 {:?}, expected {len} bytes sha256 {sha256_b64}",
                    meta.len, meta.checksum_sha256_b64
                ),
            )),
            Ok(None) => Ok(None),
            Err(err) => Err(("head_failed", format!("HeadObject {key}: {err:#}"))),
        }
    }

    /// Export, place, verify and record one partition. `None` after a keep.
    async fn copy_partition(
        &mut self,
        table: &str,
        partition: &str,
        start: &str,
        end: &str,
    ) -> Option<CopiedPartition> {
        let object = format!("{table}/{partition}");
        self.copy = None;
        let Some(order_by) = daily_export::export_order_by(table) else {
            self.keep(
                DayRepairStep::Copy,
                &object,
                "no_export_order",
                "the table has no DEDUP export order".to_string(),
            );
            return None;
        };
        let path = self
            .archiver
            .temp_dir
            .join(format!("quarantine-{table}-{partition}.csv.gz"));
        let exported = match self
            .archiver
            .export_ordered_csv(table, partition, order_by, path)
            .await
        {
            Ok(e) => e,
            Err(err) => {
                self.keep(
                    DayRepairStep::Copy,
                    &object,
                    "export_failed",
                    format!("{err:#}"),
                );
                return None;
            }
        };
        let primary = quarantine_partition_key(table, partition);
        let sidecar = quarantine_partition_sidecar_key(table, partition, &exported.gzip_sha256_hex);
        let placed = self
            .place(
                &exported.path,
                &primary,
                &sidecar,
                exported.gzip_bytes,
                &exported.gzip_sha256_b64,
            )
            .await;
        remove_temp_file(&exported.path);
        let (s3_key, reused) = match placed {
            Ok(p) => p,
            Err((stage, detail)) => {
                self.keep(DayRepairStep::Copy, &object, stage, detail);
                return None;
            }
        };
        let verified_tally = self.archiver.recount_rows(table, start, end).await;
        let s3_len = match self
            .stored_len(&s3_key, exported.gzip_bytes, &exported.gzip_sha256_b64)
            .await
        {
            Ok(len) => len,
            Err((stage, detail)) => {
                self.keep(DayRepairStep::Copy, &object, stage, detail);
                return None;
            }
        };
        let proof = match verify_archive(
            table,
            partition,
            exported.rows,
            exported.csv_bytes,
            exported.gzip_bytes,
            &exported.gzip_sha256_hex,
            verified_tally.as_ref().map(|t| t.rows),
            s3_len,
        ) {
            Ok(proof) => proof,
            Err(failure) => {
                self.keep(
                    DayRepairStep::Copy,
                    &object,
                    "verify_failed",
                    format!("{failure:?}"),
                );
                return None;
            }
        };
        let copy = QuarantinedObject {
            object: object.clone(),
            s3_key: s3_key.clone(),
            rows: Some(proof.rows),
            bytes: proof.gzip_bytes,
            sha256_hex: proof.gzip_sha256_hex.clone(),
            reused,
        };
        let mut row = DayRepairAuditRow::now(
            QUARANTINE_REPAIR_DAY,
            DayRepairStep::Copy,
            &object,
            if reused {
                DayRepairOutcome::Reused
            } else {
                DayRepairOutcome::Copied
            },
            self.run_id,
        );
        row.row_count = copy.rows;
        row.byte_count = Some(copy.bytes);
        row.sha256 = copy.sha256_hex.clone();
        row.s3_key = copy.s3_key.clone();
        if let Err(err) = self.record(&row) {
            self.keep(
                DayRepairStep::Copy,
                &object,
                "audit_failed",
                format!("copy verified but its day_repair_audit row was not acknowledged: {err:#}"),
            );
            return None;
        }
        info!(
            table,
            partition,
            rows = proof.rows,
            gzip_bytes = proof.gzip_bytes,
            s3_key = %s3_key,
            reused,
            "day repair: partition copied to quarantine and verified"
        );
        self.copy = Some(copy);
        Some(CopiedPartition {
            proof,
            s3_key,
            // Never `true`: the recount-mismatch arm of `recheck_and_drop`
            // deletes only an object the run uploaded, and this repair never
            // deletes anything (Quote 63).
            uploaded_this_run: false,
            verified_tally,
        })
    }
}

impl ArchiveIo for QuarantineIo<'_> {
    type Hold = ();

    async fn read_wal_rows(&mut self) -> Result<Vec<WalTableRow>> {
        self.archiver.fetch_wal_tables().await
    }

    async fn hold_old_hour_writers(&mut self, _wait: Duration) -> Option<Self::Hold> {
        // The repair runs with the app stopped and its WAL lock held by the
        // box subcommand, so no old-hour writer exists in any process.
        Some(())
    }

    fn in_flight_now(&mut self, _table: &str, _span: HourFrameSpan) -> InFlightVerdict {
        // Same reason: nothing can be on its way to QuestDB; the WAL gate
        // before the export and before the drop still proves it applied.
        InFlightVerdict::Clear
    }

    async fn pause_for(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn copy_and_verify(
        &mut self,
        table: &str,
        partition: &str,
        start: &str,
        end: &str,
        _summary: &mut ArchiveRunSummary,
    ) -> Option<CopiedPartition> {
        self.copy_partition(table, partition, start, end).await
    }

    async fn note_drop_intent(&mut self, _table: &str, _partition: &str) {
        // No drop tally: the repair is not the archiver and the app is down.
    }

    async fn recount_range(
        &mut self,
        table: &str,
        start: &str,
        end: &str,
    ) -> Option<super::RangeTally> {
        self.archiver.recount_rows(table, start, end).await
    }

    async fn delete_own_upload(&mut self, key: &str) -> Result<()> {
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            s3_key = key,
            "day repair: an S3 delete was asked for and REFUSED — the repair never \
             deletes an object (Quote 63)"
        );
        anyhow::bail!("day repair never deletes an S3 object (refused: {key})")
    }

    fn note_failure(
        &mut self,
        table: &str,
        partition: &str,
        outcome: ArchiveOutcome,
        detail: &str,
        _summary: &mut ArchiveRunSummary,
    ) {
        let object = format!("{table}/{partition}");
        self.keep(
            DayRepairStep::Drop,
            &object,
            outcome.as_str(),
            detail.to_string(),
        );
    }

    async fn drop_verified(&mut self, copied: CopiedPartition, _summary: &mut ArchiveRunSummary) {
        let object = format!("{}/{}", copied.proof.table(), copied.proof.partition());
        match self.archiver.drop_partition(&copied.proof).await {
            Ok(()) => {
                self.dropped = true;
                let mut row = DayRepairAuditRow::now(
                    QUARANTINE_REPAIR_DAY,
                    DayRepairStep::Drop,
                    &object,
                    DayRepairOutcome::Dropped,
                    self.run_id,
                );
                row.row_count = Some(copied.proof.rows);
                row.byte_count = Some(copied.proof.gzip_bytes);
                row.sha256 = copied.proof.gzip_sha256_hex.clone();
                row.s3_key = copied.s3_key.clone();
                // The drop already happened; a lost row is logged (coded) by
                // `record`, and the next run re-checks from QuestDB and S3.
                let _recorded = self.record(&row);
                info!(
                    object = %object,
                    rows = copied.proof.rows,
                    s3_key = %copied.s3_key,
                    "day repair: partition dropped after its verified quarantine copy"
                );
            }
            Err(err) => {
                self.keep(
                    DayRepairStep::Drop,
                    &object,
                    "drop_failed",
                    format!(
                        "{err:#} (the verified copy stays in S3; if only the answer was lost, \
                         QuestDB may already have dropped it)"
                    ),
                );
            }
        }
    }
}

impl PartitionArchiver {
    /// Copies `table`'s `partition` of 2026-11-20 to the quarantine prefix,
    /// verifies it and records it in `day_repair_audit`; with
    /// [`QuarantineMode::CopyAndDrop`] then drops it through the archiver's
    /// last-moment recount. See the module docs for what it never does.
    /// Offline: O(rows) for the export, O(bytes) for the upload.
    // TEST-EXEMPT: covered by the quarantine stub tests (test_quarantine_copy_and_drop_*)
    pub async fn quarantine_partition(
        &self,
        table: &str,
        partition: &str,
        mode: QuarantineMode,
        audit: &mut DayRepairAuditWriter,
        run_id: &str,
    ) -> QuarantineOutcome {
        let object = format!("{table}/{partition}");
        let mut io = QuarantineIo::new(self, audit, run_id);
        if !quarantine_partition_allowed(table, partition) {
            return refuse(&mut io, &object, "not_allowed");
        }
        let Some((start, end)) = partition_range_bounds(partition) else {
            return refuse(&mut io, &object, "unparseable_partition");
        };
        if !io.wal_applied(DayRepairStep::Copy, table, &object).await {
            return io.kept_outcome("wal_not_applied");
        }
        let Some(copied) = io.copy_partition(table, partition, &start, &end).await else {
            return io.kept_outcome("copy_failed");
        };
        let Some(copy) = io.copy.clone() else {
            return io.kept_outcome("copy_failed");
        };
        if mode == QuarantineMode::CopyOnly {
            return QuarantineOutcome::Copied(copy);
        }
        if !io.wal_applied(DayRepairStep::Drop, table, &object).await {
            return io.kept_outcome("wal_not_applied");
        }
        let mut summary = ArchiveRunSummary::default();
        // The step label only steers the archiver's run loop; the outcome is
        // read from what the I/O recorded.
        let _step = recheck_and_drop(
            &mut io,
            table,
            partition,
            &start,
            &end,
            copied,
            &mut summary,
        )
        .await;
        if io.dropped {
            QuarantineOutcome::Dropped(copy)
        } else {
            io.kept_outcome("drop_withheld")
        }
    }

    /// Copies one local file of the repair day (a seal, spill or DLQ file)
    /// to `{prefix}/files/{name}`, create-only, verifies the stored length
    /// and SHA-256 and records it. Never moves or deletes the local file
    /// (the caller renames it aside afterwards). O(file bytes).
    // TEST-EXEMPT: covered by test_quarantine_local_file_copies_once_then_reuses
    pub async fn quarantine_local_file(
        &self,
        path: &Path,
        name: &str,
        audit: &mut DayRepairAuditWriter,
        run_id: &str,
    ) -> QuarantineOutcome {
        let object = format!("files/{name}");
        let mut io = QuarantineIo::new(self, audit, run_id);
        if !quarantine_file_name_valid(name) {
            return refuse(&mut io, &object, "bad_file_name");
        }
        let hashed = crate::off_worker::off_worker(|| file_sha256_and_len(path));
        let (digest, len) = match hashed {
            Ok(v) => v,
            Err(err) => {
                io.keep(
                    DayRepairStep::Copy,
                    &object,
                    "read_failed",
                    format!("{}: {err}", path.display()),
                );
                return io.kept_outcome("read_failed");
            }
        };
        let sha256_hex = hex_encode(&digest);
        let sha256_b64 = base64_encode(&digest);
        let primary = quarantine_file_key(name);
        let sidecar = quarantine_file_sidecar_key(name, &sha256_hex);
        let (s3_key, reused) = match io.place(path, &primary, &sidecar, len, &sha256_b64).await {
            Ok(p) => p,
            Err((stage, detail)) => {
                io.keep(DayRepairStep::Copy, &object, stage, detail);
                return io.kept_outcome(stage);
            }
        };
        match io.stored_len(&s3_key, len, &sha256_b64).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                io.keep(
                    DayRepairStep::Copy,
                    &object,
                    "s3_object_missing",
                    format!("{s3_key} not found right after its upload"),
                );
                return io.kept_outcome("s3_object_missing");
            }
            Err((stage, detail)) => {
                io.keep(DayRepairStep::Copy, &object, stage, detail);
                return io.kept_outcome(stage);
            }
        }
        let copy = QuarantinedObject {
            object: object.clone(),
            s3_key,
            rows: None,
            bytes: len,
            sha256_hex,
            reused,
        };
        let mut row = DayRepairAuditRow::now(
            QUARANTINE_REPAIR_DAY,
            DayRepairStep::Copy,
            &object,
            if reused {
                DayRepairOutcome::Reused
            } else {
                DayRepairOutcome::Copied
            },
            run_id,
        );
        row.byte_count = Some(copy.bytes);
        row.sha256 = copy.sha256_hex.clone();
        row.s3_key = copy.s3_key.clone();
        if let Err(err) = io.record(&row) {
            io.keep(
                DayRepairStep::Copy,
                &object,
                "audit_failed",
                format!("copy verified but its day_repair_audit row was not acknowledged: {err:#}"),
            );
            return io.kept_outcome("audit_failed");
        }
        QuarantineOutcome::Copied(copy)
    }

    /// `(name, diskSize)` of every partition of `table` that this repair may
    /// copy ([`quarantine_partition_allowed`]); empty when the table is
    /// absent. One listing, O(partitions).
    ///
    /// # Errors
    /// The listing failed or could not be read.
    // TEST-EXEMPT: covered by test_repair_day_partitions_lists_only_the_repair_day
    pub async fn repair_day_partitions(&self, table: &str) -> Result<Vec<(String, u64)>> {
        let sizes = self.partition_sizes(table).await?;
        Ok(sizes
            .into_iter()
            .filter(|(p, _)| quarantine_partition_allowed(table, p))
            .collect())
    }

    /// Every object under [`QUARANTINE_PREFIX`] with its size (a dry-run
    /// report, and the resume check of a partition already gone). One
    /// paginated listing, O(objects).
    ///
    /// # Errors
    /// The listing failed.
    // TEST-EXEMPT: one paginated AWS SDK call (S3Cold::list_keys)
    pub async fn quarantine_listing(&self) -> Result<Vec<(String, u64)>> {
        self.cold.list_keys(&format!("{QUARANTINE_PREFIX}/")).await
    }

    /// The object at the archive's CANONICAL key of `table`'s `partition`,
    /// when there is one (read only: the dry run reports a canonical 20 Nov
    /// object, the repair never writes one). `Err` when S3 could not be
    /// asked, so "absent" is never a guess.
    ///
    /// # Errors
    /// The HeadObject failed.
    // TEST-EXEMPT: covered by test_canonical_object_reads_without_writing
    pub async fn canonical_object(
        &self,
        table: &str,
        partition: &str,
    ) -> Result<Option<S3ObjectMeta>> {
        self.cold
            .head(&super::canonical_archive_key(table, partition))
            .await
    }
}

/// Logs and records a refusal (best effort) and returns it.
fn refuse(io: &mut QuarantineIo<'_>, object: &str, reason: &'static str) -> QuarantineOutcome {
    error!(
        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
        object,
        reason,
        "day repair: quarantine REFUSED — outside the 2026-11-20 held-kind allowlist; \
         nothing copied, dropped or deleted"
    );
    let mut row = DayRepairAuditRow::now(
        QUARANTINE_REPAIR_DAY,
        DayRepairStep::Copy,
        object,
        DayRepairOutcome::Refused,
        io.run_id,
    );
    row.detail = reason.to_string();
    let _recorded = io.record(&row);
    QuarantineOutcome::Refused { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quarantine_partition_allowed_admits_only_held_kinds_of_the_repair_day() {
        for (table, partition) in [
            ("ticks", "2026-11-20T09"),
            ("ticks", "2026-11-20T00"),
            ("tick_quotes", "2026-11-20T15"),
            ("feed_aux_packets", "2026-11-20T10"),
            ("candles_1m", "2026-11-20"),
            ("candles_60m", "2026-11-20"),
        ] {
            assert!(
                quarantine_partition_allowed(table, partition),
                "{table} {partition} must be allowed"
            );
        }
        for (table, partition) in [
            ("market_depth", "2026-11-20T09"),
            ("market_depth_book", "2026-11-20T09"),
            ("dhan_live_crossverify_daily", "2026-11-20"),
            ("partition_archive_audit", "2026-11-20"),
            ("order_audit", "2026-11-20"),
            ("instrument_lifecycle", "2026-11-20"),
            ("day_repair_audit", "2026-11-20"),
            ("ticks", "2026-11-19T15"),
            ("ticks", "2026-11-21T00"),
            ("candles_1m", "2026-11-21"),
            ("ticks", "2026-11-20X09"),
            ("ticks", "2026-11-20T09'; DROP"),
            ("ticks", ""),
            ("ticks", "2026-11-2"),
        ] {
            assert!(
                !quarantine_partition_allowed(table, partition),
                "{table} {partition:?} must be refused"
            );
        }
    }

    #[test]
    fn test_quarantine_tables_are_held_kinds_and_never_depth() {
        let tables = quarantine_tables();
        assert!(tables.contains(&"ticks"));
        assert!(tables.contains(&"tick_quotes"));
        assert!(tables.contains(&"feed_aux_packets"));
        assert!(tables.contains(&"candles_1m"));
        assert!(
            tables
                .iter()
                .all(|t| crate::held_days::table_is_held_kind(t))
        );
        assert!(tables.iter().all(|t| !t.starts_with("market_depth")));
        for t in &tables {
            assert!(
                daily_export::export_order_by(t).is_some(),
                "{t} must have a DEDUP export order"
            );
        }
    }

    #[test]
    fn test_quarantine_keys_never_touch_the_canonical_prefix() {
        let hex = "ab".repeat(32);
        let keys = [
            quarantine_partition_key("ticks", "2026-11-20T09"),
            quarantine_partition_sidecar_key("ticks", "2026-11-20T09", &hex),
            quarantine_file_key("seal-spill/seals_v5-20261120.bin"),
            quarantine_file_sidecar_key("seal-spill/seals_v5-20261120.bin", &hex),
        ];
        for key in &keys {
            assert!(
                key.starts_with("quarantine/2026-11-20-ltt-utc-shift/"),
                "{key}"
            );
            assert!(!key.starts_with(super::super::ARCHIVE_S3_PREFIX), "{key}");
            assert!(!key.contains("/_daily/"), "{key}");
        }
        assert_eq!(
            keys[0],
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.csv.gz"
        );
        assert_ne!(keys[0], keys[1], "the sidecar never equals the primary key");
        assert_ne!(keys[2], keys[3]);
    }

    #[test]
    fn test_quarantine_partition_key_shape() {
        assert_eq!(
            quarantine_partition_key("candles_1m", "2026-11-20"),
            "quarantine/2026-11-20-ltt-utc-shift/partitions/candles_1m/2026-11-20.csv.gz"
        );
    }

    #[test]
    fn test_quarantine_partition_sidecar_key_embeds_the_digest() {
        let key = quarantine_partition_sidecar_key("ticks", "2026-11-20T09", "00ff");
        assert_eq!(
            key,
            "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.sha-00ff.csv.gz"
        );
    }

    #[test]
    fn test_quarantine_file_key_shape() {
        assert_eq!(
            quarantine_file_key("dlq/a.ndjson"),
            "quarantine/2026-11-20-ltt-utc-shift/files/dlq/a.ndjson"
        );
    }

    #[test]
    fn test_quarantine_file_sidecar_key_embeds_the_digest() {
        assert_eq!(
            quarantine_file_sidecar_key("dlq/a.ndjson", "00ff"),
            "quarantine/2026-11-20-ltt-utc-shift/files/dlq/a.ndjson.sha-00ff"
        );
    }

    #[test]
    fn test_quarantine_file_name_valid_refuses_traversal_and_odd_bytes() {
        for ok in [
            "seals_v5-20261120.bin",
            "seal-spill/replaying/seals_v5-20261120.bin.1",
            "dlq/seal-dlq-20261120.ndjson",
        ] {
            assert!(quarantine_file_name_valid(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a b",
            "a\nb",
            "é",
            "a\\b",
        ] {
            assert!(!quarantine_file_name_valid(bad), "{bad:?}");
        }
        assert!(!quarantine_file_name_valid(
            &"a".repeat(QUARANTINE_FILE_NAME_MAX_BYTES + 1)
        ));
        assert!(quarantine_file_name_valid(
            &"a".repeat(QUARANTINE_FILE_NAME_MAX_BYTES)
        ));
    }

    #[test]
    fn test_placement_step_covers_every_arm() {
        assert_eq!(
            placement_step(ExistingObject::Absent, false),
            PlacementStep::Upload
        );
        assert_eq!(
            placement_step(ExistingObject::Absent, true),
            PlacementStep::Upload
        );
        assert_eq!(
            placement_step(ExistingObject::Identical, false),
            PlacementStep::Reuse
        );
        assert_eq!(
            placement_step(ExistingObject::Identical, true),
            PlacementStep::Reuse
        );
        assert_eq!(
            placement_step(ExistingObject::Different, false),
            PlacementStep::TrySidecar
        );
        assert_eq!(
            placement_step(ExistingObject::Different, true),
            PlacementStep::Conflict
        );
    }

    #[test]
    fn test_stored_matches_needs_length_and_checksum() {
        let meta = |len, sum: Option<&str>| S3ObjectMeta {
            len,
            checksum_sha256_b64: sum.map(str::to_string),
        };
        assert!(stored_matches(&meta(10, Some("abc")), 10, "abc"));
        assert!(!stored_matches(&meta(11, Some("abc")), 10, "abc"));
        assert!(!stored_matches(&meta(10, Some("abd")), 10, "abc"));
        assert!(
            !stored_matches(&meta(10, None), 10, "abc"),
            "an object with no stored checksum proves nothing"
        );
    }

    #[test]
    fn test_file_sha256_and_len_matches_a_one_shot_digest() {
        use sha2::Digest as _;
        let dir = std::env::temp_dir().join(format!(
            "tv-quarantine-hash-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("f.bin");
        let body: Vec<u8> = (0..(FILE_HASH_CHUNK_BYTES * 2 + 17))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        std::fs::write(&path, &body).expect("write");
        let (digest, len) = file_sha256_and_len(&path).expect("hash");
        let want: [u8; 32] = sha2::Sha256::digest(&body).into();
        assert_eq!(digest, want);
        assert_eq!(len, u64::try_from(body.len()).unwrap_or(0));
        assert!(file_sha256_and_len(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
