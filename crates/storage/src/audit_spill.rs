//! Disk tier for the order-side audit writers (audit PR42b; `live_order_events`
//! joined 2026-11-12, plan item LF-5).
//!
//! # Why this exists
//!
//! `order_audit`, `pnl_audit`, `order_leg_pnl` and `live_order_events` each
//! flush one ILP batch per order event. Until 2026-10-01 a failed flush
//! DISCARDED the batch (the poisoned-buffer defense), counted and coded, so a
//! QuestDB outage during the session cost every order-side audit row written
//! in it. The rows are SEBI forensics, and a counted loss is still a loss.
//!
//! Now a failed flush writes the batch's exact ILP bytes to a file under
//! `data/spill/audit/<table>/`, and one drain task per table POSTs them back
//! to QuestDB's `/write` once it answers. A row is lost only when the disk
//! tier itself refuses it (cap reached, write failed) or QuestDB permanently
//! refuses the payload. Both are counted. For the three order-side tables a
//! refused spill reaches the order-audit chain-loss alarm on the writer's own
//! discard counter (`tv_order_audit_rows_discarded_total` for `order_audit`,
//! `tv_order_audit_chain_lost_total` for the two P&L tables). For
//! `live_order_events` it is counted on its local
//! `tv_live_order_events_rows_discarded_total` and pages through
//! `audit-rows-lost` (`tv_audit_spill_refused_rows_total`), not chain-loss
//! (noise lock §2.22-a). A permanent refusal is counted on
//! `tv_order_audit_chain_lost_total`.
//!
//! # How an outage still pages
//!
//! A spilled batch makes the writer's flush succeed, so the counters that
//! used to page on a failed flush stay at zero. The drain therefore pages
//! itself: once the oldest file still waiting is
//! [`AUDIT_SPILL_BACKLOG_PAGE_SECS`] old, it counts one
//! `tv_order_audit_persist_errors_total{stage="spill_backlog"}` (a leg of the
//! same alarm) and logs AUDIT-06, once per backlog episode.
//!
//! # Why this is not `tick_spill_replay`
//!
//! `replay_spill_dir` keeps only lines stamped inside the live session window
//! and truncates the file afterwards. That is right for ticks and wrong here:
//! an end-of-day P&L row and an after-hours order update are stamped outside
//! that window, and would be dropped without a count. This module reuses the
//! window-free helpers only (`write_url`, `list_spill_files`,
//! `is_permanent_refusal`, `describe_send_error`, `ilp_chunk_ranges`).
//!
//! # Why one immutable file per failed flush
//!
//! Each spill is written to `<name>.ilp.tmp`, synced, then renamed to
//! `<name>.ilp`, so the drain never sees a partial file and never reads a
//! file that is still growing. There is no offset bookkeeping to drift.
//!
//! # Why replay is safe to repeat
//!
//! Every row carries its own designated timestamp (`.at(..)` in each writer),
//! and each table's DEDUP key starts with that `ts` plus the row's identity
//! columns. A re-POST therefore UPSERTs identical bytes onto the same row and
//! can never overwrite a newer one. That is what makes a crash between a
//! successful POST and the file delete harmless.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use reqwest::Client;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

use crate::tick_spill_replay::{
    LineIsolation, QUARANTINE_DIR, QuarantineStreak, REPLAY_MAX_CHUNK_BYTES, SPILL_FILE_EXTENSION,
    describe_send_error, ilp_chunk_ranges, is_permanent_refusal, is_row_end, isolate_refused_lines,
    list_spill_files, set_aside_rejected_lines, write_url,
};

/// Base directory of the audit disk tier. Relative, like the tick and depth
/// spill directories next to it (`data/spill/ticks`, `data/spill/depth`).
pub const AUDIT_SPILL_BASE: &str = "data/spill/audit";

/// Byte ceiling for the files still waiting in one table directory.
///
/// An order-side row is a few hundred bytes, so this holds well over 100,000
/// rows per table: more than a full session of order events at the vendor's
/// 7,000/day order budget, several events each. Quarantined files do not
/// count: they grow only by permanent refusals, each already counted as lost
/// and paged, and letting them fill the cap would refuse every later spill.
pub const AUDIT_SPILL_MAX_BYTES_PER_TABLE: u64 = 64 * 1024 * 1024;

/// File ceiling for the files still waiting in one table directory
/// (quarantine excluded, as above).
///
/// Bounds the directory scan the cap check makes on every spill (O(files)).
pub const AUDIT_SPILL_MAX_FILES_PER_TABLE: usize = 50_000;

/// Age of the oldest waiting spill file at which the drain pages, once per
/// backlog episode. Thirty minutes: long enough that a QuestDB restart or a
/// short outage drains without a page, short enough that SEBI rows do not
/// sit on local disk unwatched for a session.
pub const AUDIT_SPILL_BACKLOG_PAGE_SECS: u64 = 1_800;

/// Seconds between drain rounds.
pub const AUDIT_SPILL_DRAIN_INTERVAL_SECS: u64 = 60;

/// Request timeout for one `/write` POST from the drain.
pub const AUDIT_SPILL_REQUEST_TIMEOUT_SECS: u64 = 30;

/// A `.tmp` file older than this is from a crash between write and rename.
/// It is set aside in quarantine, never deleted.
pub const AUDIT_SPILL_STALE_TMP_SECS: u64 = 3_600;

/// The chain-loss counter every audit loss path counts on (audit PR42a).
const CHAIN_LOST_COUNTER: &str = "tv_order_audit_chain_lost_total";

/// Extension of a spill file that is still being written.
const TMP_EXTENSION: &str = "tmp";

/// Process-wide sequence so two spills in the same nanosecond get distinct names.
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// The order-side audit tables that spill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSpillTable {
    /// `order_audit` (order consumer and Dhan push consumer both write it).
    OrderAudit,
    /// `pnl_audit`.
    PnlAudit,
    /// `order_leg_pnl`.
    OrderLegPnl,
    /// `live_order_events` (plan item LF-5, 2026-11-12): the paper order event
    /// log of the live signal-timing work.
    LiveOrderEvents,
}

impl AuditSpillTable {
    /// Every table, for metric seeding.
    pub const ALL: [Self; 4] = [
        Self::OrderAudit,
        Self::PnlAudit,
        Self::OrderLegPnl,
        Self::LiveOrderEvents,
    ];

    /// The QuestDB table name, also the spill sub-directory name.
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        match self {
            Self::OrderAudit => crate::order_audit_persistence::ORDER_AUDIT_TABLE,
            Self::PnlAudit => crate::pnl_audit_persistence::PNL_AUDIT_TABLE,
            Self::OrderLegPnl => crate::order_leg_pnl_persistence::ORDER_LEG_PNL_TABLE,
            Self::LiveOrderEvents => crate::live_order_events_persistence::LIVE_ORDER_EVENTS_TABLE,
        }
    }

    /// The `source` label this table's lost rows carry on the chain counter.
    #[must_use]
    pub const fn lost_source(self) -> &'static str {
        match self {
            Self::OrderAudit => "order_audit_spill_lost",
            Self::PnlAudit => "pnl_audit_spill_lost",
            Self::OrderLegPnl => "order_leg_pnl_spill_lost",
            Self::LiveOrderEvents => "live_order_events_spill_lost",
        }
    }

    /// The production spill directory for this table.
    #[must_use]
    // TEST-EXEMPT: exercised by the drain tests in this file, which spill and drain each table's default directory
    pub fn default_dir(self) -> PathBuf {
        Path::new(AUDIT_SPILL_BASE).join(self.table_name())
    }

    const fn index(self) -> usize {
        match self {
            Self::OrderAudit => 0,
            Self::PnlAudit => 1,
            Self::OrderLegPnl => 2,
            Self::LiveOrderEvents => 3,
        }
    }
}

/// Pre-registers every audit-spill series at zero, so the first spill of a
/// session is not consumed as a delta baseline.
// TEST-EXEMPT: metric registration only; called once from the drain task start
pub fn register_audit_spill_baseline() {
    for table in AuditSpillTable::ALL {
        let name = table.table_name();
        metrics::counter!("tv_audit_spill_rows_total", "table" => name).increment(0);
        metrics::counter!("tv_audit_spill_replayed_rows_total", "table" => name).increment(0);
        metrics::counter!("tv_audit_spill_refused_rows_total", "table" => name).increment(0);
        metrics::counter!("tv_audit_spill_quarantined_rows_total", "table" => name).increment(0);
        metrics::counter!("tv_audit_spill_replay_failed_total", "table" => name).increment(0);
        metrics::counter!(CHAIN_LOST_COUNTER, "source" => table.lost_source()).increment(0);
    }
    metrics::counter!(BACKLOG_PAGE_COUNTER, "stage" => BACKLOG_PAGE_STAGE).increment(0);
}

/// The shipped counter a stale backlog pages on: a leg of the order-audit
/// chain-loss alarm (`order-side-alarms.tf`, m2).
const BACKLOG_PAGE_COUNTER: &str = "tv_order_audit_persist_errors_total";

/// The `stage` label of a backlog page on [`BACKLOG_PAGE_COUNTER`].
const BACKLOG_PAGE_STAGE: &str = "spill_backlog";

/// One table's spill target, held by a writer.
#[derive(Debug, Clone)]
pub struct AuditSpill {
    dir: PathBuf,
    table: AuditSpillTable,
}

impl AuditSpill {
    /// The production spill target for `table`.
    #[must_use]
    // TEST-EXEMPT: one-line constructor; the spill tests in this file build the same struct and the four audit writers call it
    pub fn for_table(table: AuditSpillTable) -> Self {
        Self {
            dir: table.default_dir(),
            table,
        }
    }

    /// A spill target in an explicit directory (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn in_dir(table: AuditSpillTable, dir: PathBuf) -> Self {
        Self { dir, table }
    }

    /// The directory this target writes to (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes one failed batch (`payload`, holding `rows` whole rows) to a new
    /// immutable spill file and returns its path.
    ///
    /// O(files in the directory) for the cap check, bounded by
    /// [`AUDIT_SPILL_MAX_FILES_PER_TABLE`]; one file write, one `fsync` of the
    /// file and one of the directory. Called only after a flush has already
    /// failed, never on the steady-state path.
    ///
    /// # Errors
    /// `StorageFull` when the directory is at its byte or file ceiling,
    /// `InvalidInput` for an empty batch, or the underlying I/O error.
    pub fn spill(&self, payload: &[u8], rows: usize) -> std::io::Result<PathBuf> {
        if payload.is_empty() || rows == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty audit spill batch",
            ));
        }
        std::fs::create_dir_all(&self.dir)?;
        let (bytes, files) = dir_usage(&self.dir);
        let new_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if files >= AUDIT_SPILL_MAX_FILES_PER_TABLE
            || bytes.saturating_add(new_bytes) > AUDIT_SPILL_MAX_BYTES_PER_TABLE
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "audit spill directory at its ceiling ({bytes} bytes, {files} files; \
                     caps {AUDIT_SPILL_MAX_BYTES_PER_TABLE} bytes, \
                     {AUDIT_SPILL_MAX_FILES_PER_TABLE} files)"
                ),
            ));
        }
        let name = spill_file_name(rows);
        let final_path = self.dir.join(&name);
        let tmp_path = self.dir.join(format!("{name}.{TMP_EXTENSION}"));
        let written = write_and_sync(&tmp_path, payload)
            .and_then(|()| std::fs::rename(&tmp_path, &final_path));
        if let Err(err) = written {
            // A tmp file left behind would be quarantined as stale later;
            // remove it now so the failed spill leaves nothing half-done.
            if let Err(remove_err) = std::fs::remove_file(&tmp_path) {
                tracing::debug!(%remove_err, "could not remove a failed audit spill tmp file");
            }
            return Err(err);
        }
        // The file is complete and in place, so the drain will replay it.
        // A failed directory sync only weakens its survival of a power cut;
        // reporting the batch as refused would count rows as lost that are
        // in fact replayed.
        if let Err(err) = sync_dir(&self.dir) {
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                table = self.table.table_name(),
                path = %final_path.display(),
                %err,
                "AUDIT-06: an audit spill file is written but its directory could not be \
                 synced — the rows are kept and replayed, though a power cut now could \
                 lose the file"
            );
        }
        Ok(final_path)
    }
}

/// Spills a failed writer batch, or reports why it could not.
///
/// Returns `true` when the rows are safe on disk; the caller then clears its
/// buffer WITHOUT counting a discard. Returns `false` when there is no spill
/// target or the spill failed; the caller then discards and counts as before.
/// A refused spill is counted here and logged once with AUDIT-06.
pub fn spill_failed_batch(spill: Option<&AuditSpill>, payload: &[u8], rows: usize) -> bool {
    let Some(spill) = spill else {
        return false;
    };
    let name = spill.table.table_name();
    match spill.spill(payload, rows) {
        Ok(path) => {
            metrics::counter!("tv_audit_spill_rows_total", "table" => name).increment(rows as u64);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                table = name,
                rows,
                path = %path.display(),
                "AUDIT-06: {name} flush failed — {rows} row(s) written to the disk tier, \
                 not lost; they are replayed into QuestDB once it answers"
            );
            true
        }
        Err(err) => {
            metrics::counter!("tv_audit_spill_refused_rows_total", "table" => name)
                .increment(rows as u64);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                table = name,
                rows,
                %err,
                "AUDIT-06: {name} flush failed and the disk tier refused the batch — \
                 {rows} row(s) will be discarded"
            );
            false
        }
    }
}

/// Bytes and file count of the files directly in `dir` (waiting and `.tmp`
/// files). The quarantine sub-directory is not counted.
fn dir_usage(dir: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut files = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (bytes, files);
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            files = files.saturating_add(1);
        }
    }
    (bytes, files)
}

/// Age in seconds, at `now_nanos`, of the oldest waiting spill file in `dir`,
/// read from its name. `None` when nothing waits. O(files).
fn oldest_spill_age_secs(dir: &Path, now_nanos: i64) -> Option<u64> {
    let oldest = list_spill_files(dir).into_iter().find_map(|path| {
        path.file_name()?
            .to_str()?
            .split('-')
            .next()?
            .parse::<i64>()
            .ok()
    })?;
    let age_nanos = now_nanos.saturating_sub(oldest).max(0);
    Some(u64::try_from(age_nanos / 1_000_000_000).unwrap_or(0))
}

/// `<unix_nanos:020>-<pid>-<seq:010>-<rows>.ilp`: sorts oldest first, unique
/// per process, and carries the row count so the drain never has to parse
/// ILP (a string field may hold an escaped newline).
fn spill_file_name(rows: usize) -> String {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0);
    let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{nanos:020}-{}-{seq:010}-{rows}.{SPILL_FILE_EXTENSION}",
        std::process::id()
    )
}

/// The row count encoded in a spill file name (`.ilp`, or an unfinished
/// `.ilp.tmp`), or `None` for a foreign name.
#[must_use]
pub fn rows_in_spill_file_name(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let name = name
        .strip_suffix(&format!(".{TMP_EXTENSION}"))
        .unwrap_or(name);
    let stem = name.strip_suffix(&format!(".{SPILL_FILE_EXTENSION}"))?;
    stem.rsplit('-').next()?.parse().ok()
}

/// Mode of an audit spill file: owner read/write only. These rows are order
/// and P&L history kept for SEBI; the process default umask left them
/// readable by every local user (2026-10-11).
const AUDIT_SPILL_FILE_MODE: u32 = 0o600;

fn write_and_sync(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(AUDIT_SPILL_FILE_MODE);
    }
    let mut file = opts.open(path)?;
    file.write_all(payload)?;
    file.sync_all()
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// What one drain round did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AuditDrainOutcome {
    /// Files fully accepted by QuestDB and deleted.
    pub files_replayed: u64,
    /// Rows in those files.
    pub rows_replayed: u64,
    /// Files QuestDB permanently refused, moved to quarantine.
    pub files_quarantined: u64,
    /// Rows QuestDB refused one by one (line isolation) and set aside in
    /// `quarantine/<file>.rejected-lines`; the rest of their file landed.
    pub rows_isolated: u64,
    /// The round stopped after [`crate::tick_spill_replay::REPLAY_QUARANTINE_STREAK_STOP`]
    /// files in a row were quarantined with the same status: the server is
    /// refusing the table, not a file, so the rest wait for a fix (B1).
    pub stopped_on_refusal_streak: bool,
    /// The round ended early (a transient failure, or the refusal streak
    /// above); the remaining files stay for the next round.
    pub failed: bool,
}

/// Replays every spill file in `dir` into QuestDB, oldest first.
///
/// O(files) per round. A 2xx deletes the file. A permanent refusal (400, 413
/// or 422) first isolates the refused rows by bisection, as the tick replay
/// does (B1, 2026-11-13): every row QuestDB takes lands, only the rows it
/// refuses one by one are set aside in `quarantine/<file>.rejected-lines`,
/// and the file is then deleted. A refusal no row of the file survives (or
/// past the isolation caps) moves the whole file to `quarantine/` and counts
/// the rows that did not land as lost (AUDIT-06); after
/// [`crate::tick_spill_replay::REPLAY_QUARANTINE_STREAK_STOP`] such files in
/// a row with the same status the round stops, so a refusal of the whole
/// table does not set the backlog aside. The drain loop carries that streak
/// across rounds (B1 review): while the same status keeps coming back, later
/// rounds keep the next file in place and stop, so at most that many files
/// are ever set aside until a file lands again. Anything else ends the round with
/// the file kept, so a struggling QuestDB is not pushed harder; the next
/// round starts from the same file. No session-window filter is applied:
/// every row is replayed as written.
// TEST-EXEMPT: covered by the drain tests in this file (replay, refusal and quarantine outcomes)
pub async fn drain_audit_spill_dir(
    dir: &Path,
    table: AuditSpillTable,
    url: &str,
    client: &Client,
) -> AuditDrainOutcome {
    drain_with_streak(dir, table, url, client, &mut QuarantineStreak::default()).await
}

/// Whether a whole-file refusal with `status` continues a refusal streak that
/// ALREADY reached [`crate::tick_spill_replay::REPLAY_QUARANTINE_STREAK_STOP`]
/// (B1 review, 2026-11-13). Such a file is kept in place, not set aside: the
/// server is refusing the table, and the streak lives across rounds, so the
/// backlog waits for a fix instead of draining into quarantine three files a
/// round. Pure, O(1).
fn streak_already_stopped_for(streak: QuarantineStreak, status: u16) -> bool {
    let mut next = streak;
    streak.count() >= crate::tick_spill_replay::REPLAY_QUARANTINE_STREAK_STOP
        && next.record(status)
        && next.count() > streak.count()
}

/// [`drain_audit_spill_dir`] with the refusal streak carried in from earlier
/// rounds (the drain loop keeps one per table). A file that lands resets it.
async fn drain_with_streak(
    dir: &Path,
    table: AuditSpillTable,
    url: &str,
    client: &Client,
    streak: &mut QuarantineStreak,
) -> AuditDrainOutcome {
    let name = table.table_name();
    let mut outcome = AuditDrainOutcome::default();
    // B1 review 2: whether this round already kept a refused head file in
    // place and went on to probe the next file once.
    let mut probed_past_head = false;
    quarantine_stale_tmp_files(dir, table);
    for path in crate::off_worker::off_worker(|| list_spill_files(dir)) {
        let payload = match tokio::fs::read(&path).await {
            Ok(payload) => payload,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    table = name,
                    path = %path.display(),
                    %err,
                    "AUDIT-06: could not read an audit spill file — kept for the next round"
                );
                outcome.failed = true;
                break;
            }
        };
        let rows = rows_in_spill_file_name(&path).unwrap_or(0);
        if payload.is_empty() {
            if let Err(err) = tokio::fs::remove_file(&path).await {
                tracing::debug!(%err, "could not remove an empty audit spill file");
            }
            continue;
        }
        match post_file(&payload, url, client, dir, &path).await {
            PostVerdict::Accepted { rejected_rows } => {
                streak.reset();
                if probed_past_head {
                    // The probe landed: this round did not stop on the streak.
                    outcome.stopped_on_refusal_streak = false;
                    outcome.failed = false;
                }
                let rows = rows.saturating_sub(rejected_rows);
                if let Err(err) = remove_replayed_file(&path).await {
                    // Re-POSTed next round; the DEDUP key makes that harmless.
                    // B1 review: the isolated rows are NOT counted here; the
                    // round that removes the file counts them, once. (The
                    // `.rejected-lines` file can gain a second copy of them.)
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        table = name,
                        path = %path.display(),
                        %err,
                        "AUDIT-06: replayed an audit spill file but could not delete it — \
                         it will be re-sent (idempotent)"
                    );
                    outcome.failed = true;
                    break;
                }
                if rejected_rows > 0 {
                    outcome.rows_isolated = outcome.rows_isolated.saturating_add(rejected_rows);
                    metrics::counter!("tv_audit_spill_quarantined_rows_total", "table" => name)
                        .increment(rejected_rows);
                    metrics::counter!(CHAIN_LOST_COUNTER, "source" => table.lost_source())
                        .increment(rejected_rows);
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        table = name,
                        rows = rejected_rows,
                        path = %path.display(),
                        "AUDIT-06: QuestDB refused {rejected_rows} row(s) of an audit spill \
                         file one by one; they are kept in quarantine (.rejected-lines), the \
                         rest of the file was replayed"
                    );
                }
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                outcome.rows_replayed = outcome.rows_replayed.saturating_add(rows);
                metrics::counter!("tv_audit_spill_replayed_rows_total", "table" => name)
                    .increment(rows);
            }
            PostVerdict::Refused { status, .. } if streak_already_stopped_for(*streak, status) => {
                // B1 review: the table is still refused the same way as when
                // the streak stopped an earlier round; keep this file where
                // it is (re-probed once a round) instead of setting it aside.
                outcome.stopped_on_refusal_streak = true;
                outcome.failed = true;
                // B1 review 2: one file past it is probed once a round. If it
                // lands, the refusals were per file, not the table: the streak
                // resets (below), the good files behind drain now, and the
                // kept file is judged anew (and set aside) next round. Before,
                // four bad files in a row blocked every later file for good.
                if !probed_past_head {
                    probed_past_head = true;
                    continue;
                }
                break;
            }
            PostVerdict::Refused {
                status,
                accepted_rows,
            } => match quarantine(dir, &path) {
                Ok(target) => {
                    // Rows of earlier chunks of the same file already landed;
                    // only the rest are out of the database (B1).
                    let rows = rows.saturating_sub(accepted_rows);
                    outcome.files_quarantined = outcome.files_quarantined.saturating_add(1);
                    metrics::counter!("tv_audit_spill_quarantined_rows_total", "table" => name)
                        .increment(rows);
                    metrics::counter!(CHAIN_LOST_COUNTER, "source" => table.lost_source())
                        .increment(rows);
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        table = name,
                        status,
                        rows,
                        accepted_rows,
                        path = %target.display(),
                        "AUDIT-06: QuestDB permanently refused an audit spill file — \
                         {rows} row(s) set aside in quarantine (kept, not deleted)"
                    );
                    if streak.record(status) {
                        outcome.stopped_on_refusal_streak = true;
                        outcome.failed = true;
                        error!(
                            code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                            table = name,
                            status,
                            "AUDIT-06: QuestDB refused several {name} spill files in a row the \
                             same way; the round stops and the rest of the backlog waits on \
                             disk for a fix instead of being set aside"
                        );
                        break;
                    }
                }
                Err(err) => {
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        table = name,
                        status,
                        path = %path.display(),
                        %err,
                        "AUDIT-06: QuestDB refused an audit spill file and it could not be \
                         set aside — the round stops so it is not skipped silently"
                    );
                    outcome.failed = true;
                    break;
                }
            },
            PostVerdict::Transient => {
                outcome.failed = true;
                break;
            }
            PostVerdict::SetAsideFailed(err) => {
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    table = name,
                    path = %path.display(),
                    %err,
                    "AUDIT-06: QuestDB refused some rows of an audit spill file and they \
                     could not be set aside; the file is kept and the round stops"
                );
                outcome.failed = true;
                break;
            }
        }
    }
    outcome
}

enum PostVerdict {
    /// Every row landed except `rejected_rows`, which were set aside.
    Accepted {
        rejected_rows: u64,
    },
    /// Quarantine the whole file; `accepted_rows` of its rows already landed.
    Refused {
        status: u16,
        accepted_rows: u64,
    },
    Transient,
    /// Isolated rows could not be written to quarantine; keep the file.
    SetAsideFailed(std::io::Error),
}

/// Rows in `bytes`: row-ending newlines, plus a final row with none. An
/// escaped newline inside a string field does not count. O(bytes), cold.
fn count_rows(bytes: &[u8]) -> u64 {
    // O(1) EXEMPT: begin: cold replay path, one pass over a chunk.
    let ends = (0..bytes.len()).filter(|&i| is_row_end(bytes, i)).count();
    // O(1) EXEMPT: end
    let torn_tail = bytes.last().is_some_and(|b| *b != b'\n');
    u64::try_from(ends)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(torn_tail))
}

async fn post_file(
    payload: &[u8],
    url: &str,
    client: &Client,
    dir: &Path,
    path: &Path,
) -> PostVerdict {
    let mut accepted_rows = 0_u64;
    let mut rejected_rows = 0_u64;
    for range in ilp_chunk_ranges(payload, REPLAY_MAX_CHUNK_BYTES) {
        let chunk = bytes::Bytes::copy_from_slice(&payload[range]);
        match client.post(url).body(chunk.clone()).send().await {
            Ok(response) if response.status().is_success() => {
                accepted_rows = accepted_rows.saturating_add(count_rows(&chunk));
            }
            Ok(response) => {
                let status = response.status().as_u16();
                if !is_permanent_refusal(status) {
                    return PostVerdict::Transient;
                }
                // B1 (2026-11-13): find the refused rows instead of setting
                // the whole file aside for one bad row.
                match isolate_refused_lines(client, url, &chunk, accepted_rows > 0).await {
                    LineIsolation::Isolated(ranges) => {
                        if !ranges.is_empty()
                            && let Err(err) = crate::off_worker::off_worker(|| {
                                set_aside_rejected_lines(dir, path, &chunk, &ranges)
                            })
                        {
                            return PostVerdict::SetAsideFailed(err);
                        }
                        // O(1) EXEMPT: cold, at most the isolation's line cap.
                        let lost: u64 = ranges
                            .iter()
                            .map(|r| count_rows(chunk.get(r.clone()).unwrap_or_default()))
                            .sum();
                        rejected_rows = rejected_rows.saturating_add(lost);
                        accepted_rows =
                            accepted_rows.saturating_add(count_rows(&chunk).saturating_sub(lost));
                    }
                    LineIsolation::WholeFile => {
                        return PostVerdict::Refused {
                            status,
                            accepted_rows,
                        };
                    }
                    LineIsolation::Transient => return PostVerdict::Transient,
                }
            }
            Err(err) => {
                tracing::debug!(
                    kind = describe_send_error(&err),
                    "audit spill POST failed in transit"
                );
                return PostVerdict::Transient;
            }
        }
    }
    PostVerdict::Accepted { rejected_rows }
}

fn quarantine(dir: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let target_dir = dir.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&target_dir)?;
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unnamed.ilp")); // APPROVED: infallible fallback, no panic on a pathological path
    let target = target_dir.join(file_name);
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Moves `.tmp` files older than [`AUDIT_SPILL_STALE_TMP_SECS`] to quarantine.
/// They are from a crash between write and rename and may be partial.
fn quarantine_stale_tmp_files(dir: &Path, table: AuditSpillTable) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let stale = Duration::from_secs(AUDIT_SPILL_STALE_TMP_SECS);
    for path in entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
    {
        let is_tmp = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(TMP_EXTENSION));
        let old_enough = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= stale);
        if is_tmp && old_enough && path.is_file() {
            let moved = quarantine(dir, &path);
            // The process crashed before `spill` returned, so these rows were
            // never counted anywhere; they are not replayed, so they are lost.
            let rows = rows_in_spill_file_name(&path).unwrap_or(0);
            if moved.is_ok() {
                metrics::counter!(CHAIN_LOST_COUNTER, "source" => table.lost_source())
                    .increment(rows);
            }
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                table = table.table_name(),
                path = %path.display(),
                rows,
                moved = moved.is_ok(),
                "AUDIT-06: an audit spill file was never finished (crash between write and \
                 rename) — {rows} row(s) set aside in quarantine for a manual look, not \
                 replayed, and counted as lost"
            );
        }
    }
}

/// One flag per table, so the drain is spawned once however many writers
/// share the table.
static DRAIN_SPAWNED: [AtomicBool; AuditSpillTable::ALL.len()] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

/// Spawns `table`'s drain task once per process. Later calls are no-ops.
///
/// The drain re-runs the table's `ensure_*_table` before it replays a
/// backlog, so a boot whose own ensure failed cannot let `/write`
/// auto-create the table without its DEDUP key.
///
/// Returns `true` when this call spawned the task.
pub fn spawn_audit_spill_drain_once(table: AuditSpillTable, questdb: &QuestDbConfig) -> bool {
    if DRAIN_SPAWNED[table.index()].swap(true, Ordering::AcqRel) {
        return false;
    }
    register_audit_spill_baseline();
    let url = audit_write_url(questdb);
    let dir = table.default_dir();
    tokio::spawn(run_drain_loop(dir, table, url, questdb.clone()));
    true
}

/// The drain's `/write` URL with nanosecond precision stated, the unit every
/// writer's `.at(..)` stamps, rather than relying on the server default.
fn audit_write_url(questdb: &QuestDbConfig) -> String {
    format!(
        "{}?precision=n",
        write_url(&questdb.host, questdb.http_port)
    )
}

/// Runs the table's own idempotent `ensure_*_table` (create, add missing
/// columns, enable the DEDUP key). Returns whether the table is keyed.
async fn ensure_table(table: AuditSpillTable, questdb: &QuestDbConfig) -> bool {
    match table {
        AuditSpillTable::OrderAudit => {
            crate::order_audit_persistence::ensure_order_audit_table(questdb).await
        }
        AuditSpillTable::PnlAudit => {
            crate::pnl_audit_persistence::ensure_pnl_audit_table(questdb).await
        }
        AuditSpillTable::OrderLegPnl => {
            crate::order_leg_pnl_persistence::ensure_order_leg_pnl_table(questdb).await
        }
        AuditSpillTable::LiveOrderEvents => {
            crate::live_order_events_persistence::ensure_live_order_events_table(questdb).await
        }
    }
}

/// Edge latch for the stale-backlog page: one page per backlog episode.
#[derive(Debug, Default)]
struct BacklogPager {
    paged: bool,
}

impl BacklogPager {
    /// Feeds one observation of the oldest waiting file's age. Returns `true`
    /// on the round that crosses [`AUDIT_SPILL_BACKLOG_PAGE_SECS`]; re-arms
    /// once nothing waits.
    fn observe(&mut self, oldest_age_secs: Option<u64>) -> bool {
        match oldest_age_secs {
            None => {
                self.paged = false;
                false
            }
            Some(age) if age >= AUDIT_SPILL_BACKLOG_PAGE_SECS && !self.paged => {
                self.paged = true;
                true
            }
            Some(_) => false,
        }
    }
}

/// Drains at once, then every [`AUDIT_SPILL_DRAIN_INTERVAL_SECS`]. Never
/// returns. A failing round is logged on its rising edge only; recovery is
/// one `info!`.
async fn run_drain_loop(dir: PathBuf, table: AuditSpillTable, url: String, questdb: QuestDbConfig) {
    let name = table.table_name();
    let interval = Duration::from_secs(AUDIT_SPILL_DRAIN_INTERVAL_SECS);
    let client = loop {
        match crate::http_client::build_probe_client(AUDIT_SPILL_REQUEST_TIMEOUT_SECS) {
            Ok(client) => break client,
            Err(err) => {
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    table = name,
                    %err,
                    "AUDIT-06: the audit spill drain could not build an HTTP client — \
                     spilled rows stay on disk; retrying"
                );
                tokio::time::sleep(interval).await;
            }
        }
    };
    let mut failing = false;
    // Ensured once per backlog episode: re-run after a failed round or once
    // the directory has emptied, never on every quiet round.
    let mut ensured = false;
    let mut pager = BacklogPager::default();
    // B1 review: the refusal streak lives across rounds, so a table QuestDB
    // keeps refusing sets aside at most REPLAY_QUARANTINE_STREAK_STOP files
    // in all, not that many every round.
    let mut streak = QuarantineStreak::default();
    loop {
        // Directory scans run off the shared worker (sweep S5): a slow disk
        // must not hold the worker the frame drain and the readers use.
        if crate::off_worker::off_worker(|| list_spill_files(&dir)).is_empty() {
            ensured = false;
            // A crash between write and rename leaves only a `.tmp`; it is
            // still set aside and counted when nothing else waits.
            crate::off_worker::off_worker(|| quarantine_stale_tmp_files(&dir, table));
        } else {
            if !ensured {
                ensured = ensure_table(table, &questdb).await;
            }
            // Audit J3 (2026-10-26): a table QuestDB has not confirmed keyed is
            // never replayed into, because a replay into an ILP-auto-created
            // table would land with no DEDUP key. The rows stay on disk and the
            // round counts as failed, so the ensure runs again next round and
            // the backlog page still fires if it never clears.
            let outcome = if ensured {
                drain_with_streak(&dir, table, &url, &client, &mut streak).await
            } else {
                AuditDrainOutcome {
                    failed: true,
                    ..AuditDrainOutcome::default()
                }
            };
            if outcome.failed {
                // The rows stay on disk; this counts rounds that could not finish.
                metrics::counter!("tv_audit_spill_replay_failed_total", "table" => name)
                    .increment(1);
                ensured = false;
                if !failing {
                    error!(
                        code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                        table = name,
                        "AUDIT-06: QuestDB is not accepting the {name} spill backlog — the \
                         rows stay on disk and are retried every \
                         {AUDIT_SPILL_DRAIN_INTERVAL_SECS}s"
                    );
                }
            }
            if !outcome.failed && failing {
                info!(table = name, "the {name} spill backlog drains again");
            }
            failing = outcome.failed;
            if outcome.files_replayed > 0 {
                info!(
                    table = name,
                    files = outcome.files_replayed,
                    rows = outcome.rows_replayed,
                    "replayed {name} rows from the disk tier into QuestDB"
                );
            }
        }
        let now_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let oldest = crate::off_worker::off_worker(|| oldest_spill_age_secs(&dir, now_nanos));
        if pager.observe(oldest) {
            metrics::counter!(BACKLOG_PAGE_COUNTER, "stage" => BACKLOG_PAGE_STAGE).increment(1);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                table = name,
                oldest_age_secs = oldest.unwrap_or(0),
                "AUDIT-06: {name} rows have waited on local disk for at least \
                 {AUDIT_SPILL_BACKLOG_PAGE_SECS}s — QuestDB is not taking the backlog; \
                 the rows are kept, not lost, but are not yet in the database"
            );
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A one-shot HTTP server for the drain tests.

    use std::time::{Duration, Instant};

    /// Serves `n` requests with the raw `status` response (or stops after
    /// five seconds) and returns how many it served.
    pub(crate) fn status_server(
        status: &'static str,
        n: usize,
    ) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("set_nonblocking");
        let handle = std::thread::spawn(move || {
            let give_up_at = Instant::now() + Duration::from_secs(5);
            let mut served = 0usize;
            while served < n && Instant::now() < give_up_at {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let _ = sock.set_nonblocking(false);
                        let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut buf = [0u8; 65_536];
                        let _ = sock.read(&mut buf);
                        let _ = sock.write_all(status.as_bytes());
                        let _ = sock.flush();
                        served += 1;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            served
        });
        (format!("http://127.0.0.1:{port}/write"), handle)
    }

    /// A unique scratch directory removed on drop.
    pub(crate) struct TestDir(std::path::PathBuf);

    impl TestDir {
        pub(crate) fn new() -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let dir = std::env::temp_dir()
                .join(format!("tv-audit-spill-{}-{nanos}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create test dir");
            Self(dir)
        }

        pub(crate) fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) const OK_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
    pub(crate) const BAD_400: &str =
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    pub(crate) const ERR_500: &str =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
}

#[cfg(test)]
mod tests {
    use super::test_support::{BAD_400, ERR_500, OK_204, TestDir, status_server};
    use super::*;

    const ROWS: &[u8] =
        b"order_audit,feed=dhan detail=\"a\\\nb\" 1\norder_audit,feed=dhan x=1i 2\n";

    fn client() -> Client {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client")
    }

    #[test]
    fn spill_writes_the_exact_bytes_under_a_name_carrying_the_row_count() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let path = spill.spill(ROWS, 2).expect("spill");
        assert_eq!(std::fs::read(&path).expect("read"), ROWS);
        assert_eq!(rows_in_spill_file_name(&path), Some(2));
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some(SPILL_FILE_EXTENSION)
        );
        // No tmp file is left behind.
        assert_eq!(list_spill_files(tmp.path()), vec![path]);
        assert_eq!(std::fs::read_dir(tmp.path()).expect("dir").count(), 1);
    }

    // Regression: 2026-10-11 — audit spill files took the process umask, so
    // order and P&L rows kept for SEBI were readable by every local user.
    #[cfg(unix)]
    #[test]
    fn test_regression_audit_spill_file_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderLegPnl, tmp.path().to_path_buf());
        let path = spill.spill(ROWS, 2).expect("spill");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, AUDIT_SPILL_FILE_MODE, "got {mode:o}");
    }

    #[test]
    fn spill_names_sort_oldest_first_and_never_collide() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::PnlAudit, tmp.path().to_path_buf());
        let first = spill.spill(b"a 1\n", 1).expect("first");
        let second = spill.spill(b"b 2\n", 1).expect("second");
        assert_ne!(first, second);
        assert_eq!(list_spill_files(tmp.path()), vec![first, second]);
    }

    #[test]
    fn spill_refuses_an_empty_batch() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        assert!(spill.spill(b"", 0).is_err());
        assert!(spill.spill(ROWS, 0).is_err());
    }

    #[test]
    fn spill_refuses_past_the_byte_ceiling_of_waiting_files() {
        let tmp = TestDir::new();
        let big = std::fs::File::create(tmp.path().join("00000000000000000001-1-0000000000-1.ilp"))
            .expect("create");
        big.set_len(AUDIT_SPILL_MAX_BYTES_PER_TABLE)
            .expect("set_len");
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let err = spill.spill(ROWS, 2).expect_err("must refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
        assert_eq!(list_spill_files(tmp.path()).len(), 1);
    }

    #[test]
    fn a_full_quarantine_never_refuses_a_new_spill() {
        let tmp = TestDir::new();
        let q = tmp.path().join(QUARANTINE_DIR);
        std::fs::create_dir_all(&q).expect("mkdir");
        let big = std::fs::File::create(q.join("old.ilp")).expect("create");
        big.set_len(AUDIT_SPILL_MAX_BYTES_PER_TABLE.saturating_mul(2))
            .expect("set_len");
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let path = spill.spill(ROWS, 2).expect("quarantine does not count");
        assert_eq!(list_spill_files(tmp.path()), vec![path]);
    }

    #[test]
    fn oldest_spill_age_reads_the_oldest_name_and_none_when_empty() {
        let tmp = TestDir::new();
        assert_eq!(oldest_spill_age_secs(tmp.path(), 10_000_000_000), None);
        std::fs::write(
            tmp.path().join("00000000003000000000-1-0000000001-1.ilp"),
            b"a 1\n",
        )
        .expect("newer");
        std::fs::write(
            tmp.path().join("00000000001000000000-1-0000000000-1.ilp"),
            b"a 1\n",
        )
        .expect("older");
        // A quarantined or foreign file is not a waiting file.
        std::fs::write(tmp.path().join("0-1-1-1.ilp.tmp"), b"a 1\n").expect("tmp");
        assert_eq!(oldest_spill_age_secs(tmp.path(), 10_000_000_000), Some(9));
        // A clock behind the name reads as age 0, never a wrap.
        assert_eq!(oldest_spill_age_secs(tmp.path(), 0), Some(0));
    }

    #[test]
    fn backlog_pager_pages_once_per_episode_and_rearms_when_empty() {
        let mut pager = BacklogPager::default();
        assert!(!pager.observe(None));
        assert!(!pager.observe(Some(AUDIT_SPILL_BACKLOG_PAGE_SECS - 1)));
        assert!(pager.observe(Some(AUDIT_SPILL_BACKLOG_PAGE_SECS)));
        assert!(!pager.observe(Some(AUDIT_SPILL_BACKLOG_PAGE_SECS * 10)));
        assert!(!pager.observe(None));
        assert!(pager.observe(Some(AUDIT_SPILL_BACKLOG_PAGE_SECS + 1)));
    }

    #[test]
    fn the_drain_url_states_nanosecond_precision() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        assert_eq!(
            audit_write_url(&cfg),
            "http://tv-questdb:9000/write?precision=n"
        );
    }

    #[test]
    fn spill_failed_batch_reports_false_with_no_target_or_a_refusal() {
        assert!(!spill_failed_batch(None, ROWS, 2));
        let tmp = TestDir::new();
        // A FILE where the directory should be makes create_dir_all fail.
        let blocker = tmp.path().join("blocked");
        std::fs::write(&blocker, b"x").expect("write");
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, blocker);
        assert!(!spill_failed_batch(Some(&spill), ROWS, 2));
        let ok = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().join("ok"));
        assert!(spill_failed_batch(Some(&ok), ROWS, 2));
    }

    #[test]
    fn rows_in_spill_file_name_reads_the_last_segment() {
        assert_eq!(
            rows_in_spill_file_name(Path::new("00000000000000000001-7-0000000003-12.ilp")),
            Some(12)
        );
        assert_eq!(rows_in_spill_file_name(Path::new("garbage.ilp")), None);
        // An unfinished file still names its rows, so its loss can be counted.
        assert_eq!(
            rows_in_spill_file_name(Path::new("00000000000000000001-7-0000000003-12.ilp.tmp")),
            Some(12)
        );
        assert_eq!(
            rows_in_spill_file_name(Path::new("00000000000000000001-7-0000000003-12.txt")),
            None
        );
    }

    #[test]
    fn every_table_has_a_distinct_dir_and_loss_source() {
        let dirs: Vec<PathBuf> = AuditSpillTable::ALL
            .iter()
            .map(|t| t.default_dir())
            .collect();
        assert_eq!(dirs[0], Path::new("data/spill/audit/order_audit"));
        assert_eq!(dirs[1], Path::new("data/spill/audit/pnl_audit"));
        assert_eq!(dirs[2], Path::new("data/spill/audit/order_leg_pnl"));
        assert_eq!(dirs[3], Path::new("data/spill/audit/live_order_events"));
        let mut sources: Vec<&str> = AuditSpillTable::ALL
            .iter()
            .map(|t| t.lost_source())
            .collect();
        sources.sort_unstable();
        sources.dedup();
        assert_eq!(sources.len(), 4);
        let indexes: Vec<usize> = AuditSpillTable::ALL.iter().map(|t| t.index()).collect();
        assert_eq!(indexes, vec![0, 1, 2, 3]);
        assert_eq!(DRAIN_SPAWNED.len(), AuditSpillTable::ALL.len());
    }

    #[tokio::test]
    async fn drain_deletes_each_file_questdb_accepts_in_order() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        spill.spill(ROWS, 2).expect("one");
        spill.spill(b"order_audit x=1i 3\n", 1).expect("two");
        let (url, server) = status_server(OK_204, 2);
        let outcome =
            drain_audit_spill_dir(tmp.path(), AuditSpillTable::OrderAudit, &url, &client()).await;
        assert_eq!(server.join().expect("join"), 2);
        assert_eq!(outcome.files_replayed, 2);
        assert_eq!(outcome.rows_replayed, 3);
        assert!(!outcome.failed);
        assert!(list_spill_files(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn drain_keeps_the_files_when_questdb_is_unreachable() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::PnlAudit, tmp.path().to_path_buf());
        let path = spill.spill(ROWS, 2).expect("spill");
        let outcome = drain_audit_spill_dir(
            tmp.path(),
            AuditSpillTable::PnlAudit,
            "http://127.0.0.1:1/write",
            &client(),
        )
        .await;
        assert!(outcome.failed);
        assert_eq!(outcome.files_replayed, 0);
        assert_eq!(list_spill_files(tmp.path()), vec![path]);
    }

    #[tokio::test]
    async fn drain_stops_the_round_on_a_server_error_and_keeps_every_file() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderLegPnl, tmp.path().to_path_buf());
        spill.spill(ROWS, 2).expect("one");
        spill.spill(ROWS, 2).expect("two");
        let (url, server) = status_server(ERR_500, 1);
        let outcome =
            drain_audit_spill_dir(tmp.path(), AuditSpillTable::OrderLegPnl, &url, &client()).await;
        assert_eq!(
            server.join().expect("join"),
            1,
            "one POST, then the round stops"
        );
        assert!(outcome.failed);
        assert_eq!(list_spill_files(tmp.path()).len(), 2);
    }

    #[tokio::test]
    async fn drain_quarantines_a_permanently_refused_file_and_moves_on() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let bad = spill.spill(ROWS, 2).expect("bad");
        // B1: the whole-file POST, then one POST per row while isolating;
        // no row is taken, so the whole file is set aside as before.
        let (url, server) = status_server(BAD_400, 3);
        let outcome =
            drain_audit_spill_dir(tmp.path(), AuditSpillTable::OrderAudit, &url, &client()).await;
        assert_eq!(server.join().expect("join"), 3);
        assert_eq!(outcome.files_quarantined, 1);
        assert!(!outcome.failed);
        assert!(list_spill_files(tmp.path()).is_empty());
        let moved = tmp
            .path()
            .join(QUARANTINE_DIR)
            .join(bad.file_name().expect("name"));
        assert_eq!(
            std::fs::read(moved).expect("kept"),
            ROWS,
            "kept, not deleted"
        );
    }

    /// Answers 400 to a request whose body holds `BAD`, 204 otherwise, for
    /// up to `n` requests; returns how many it served.
    fn judging_server(n: usize) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("set_nonblocking");
        let handle = std::thread::spawn(move || {
            let give_up_at = std::time::Instant::now() + Duration::from_secs(5);
            let mut served = 0usize;
            while served < n && std::time::Instant::now() < give_up_at {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let _ = sock.set_nonblocking(false);
                        let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
                        let mut seen = Vec::new();
                        let mut buf = [0u8; 65_536];
                        // Read until the body arrives (the head may come alone).
                        while let Ok(k) = sock.read(&mut buf) {
                            if k == 0 {
                                break;
                            }
                            seen.extend_from_slice(&buf[..k]);
                            let head_end = seen.windows(4).position(|w| w == b"\r\n\r\n");
                            if let Some(h) = head_end {
                                let head = String::from_utf8_lossy(&seen[..h]).to_lowercase();
                                let len = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if seen.len() >= h + 4 + len {
                                    break;
                                }
                            }
                        }
                        let bad = seen.windows(3).any(|w| w == b"BAD");
                        let reply = if bad { BAD_400 } else { OK_204_CLOSE };
                        let _ = sock.write_all(reply.as_bytes());
                        let _ = sock.flush();
                        served += 1;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            served
        });
        (format!("http://127.0.0.1:{port}/write"), handle)
    }

    const OK_204_CLOSE: &str =
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    /// Regression, B1 (2026-11-13): one refused row no longer sets its whole
    /// file aside. The good rows (one with a newline escaped inside a string)
    /// land, only the refused row is kept in `.rejected-lines`, and the file
    /// is gone. Before, the whole file went to quarantine and every row in it
    /// was counted lost.
    #[tokio::test]
    async fn test_regression_drain_isolates_one_refused_row_and_replays_the_rest() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let rows: &[u8] = b"order_audit,feed=dhan detail=\"a\\\nb\" 1\n\
            order_audit,feed=dhan BAD=1i 2\norder_audit,feed=dhan x=1i 3\n";
        let path = spill.spill(rows, 3).expect("spill");
        let (url, server) = judging_server(64);
        let outcome =
            drain_audit_spill_dir(tmp.path(), AuditSpillTable::OrderAudit, &url, &client()).await;
        drop(server);
        assert!(!outcome.failed, "{outcome:?}");
        assert_eq!(
            outcome.files_quarantined, 0,
            "the file is not set aside whole"
        );
        assert_eq!(outcome.files_replayed, 1);
        assert_eq!(outcome.rows_replayed, 2);
        assert_eq!(outcome.rows_isolated, 1);
        assert!(list_spill_files(tmp.path()).is_empty());
        let kept = tmp.path().join(QUARANTINE_DIR).join(format!(
            "{}{}",
            path.file_name().and_then(|n| n.to_str()).expect("name"),
            crate::tick_spill_replay::REJECTED_LINES_SUFFIX
        ));
        assert_eq!(
            std::fs::read(kept).expect("the refused row is kept"),
            b"order_audit,feed=dhan BAD=1i 2\n"
        );
    }

    /// Regression, B1 (2026-11-13): when QuestDB refuses every file the same
    /// way (a column type changed), the round stops after three files in a
    /// row instead of setting the whole backlog aside.
    #[tokio::test]
    async fn test_regression_drain_stops_after_three_files_refused_the_same_way() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::PnlAudit, tmp.path().to_path_buf());
        for _ in 0..5 {
            spill.spill(b"pnl_audit BAD=1i 1\n", 1).expect("spill");
        }
        let (url, server) = judging_server(64);
        let outcome =
            drain_audit_spill_dir(tmp.path(), AuditSpillTable::PnlAudit, &url, &client()).await;
        drop(server);
        assert_eq!(
            outcome.files_quarantined,
            u64::from(crate::tick_spill_replay::REPLAY_QUARANTINE_STREAK_STOP)
        );
        assert!(outcome.stopped_on_refusal_streak);
        assert!(outcome.failed, "the round ends early");
        assert_eq!(
            list_spill_files(tmp.path()).len(),
            2,
            "the rest wait on disk"
        );
    }

    /// Regression, B1 review (2026-11-13): the refusal streak lives across
    /// rounds (the drain loop keeps it). Before, every 60 s round started a
    /// fresh streak and set three more files aside, so a table QuestDB kept
    /// refusing drained its whole backlog into quarantine within minutes.
    #[tokio::test]
    async fn test_regression_refusal_streak_holds_across_rounds() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::PnlAudit, tmp.path().to_path_buf());
        for _ in 0..6 {
            spill.spill(b"pnl_audit BAD=1i 1\n", 1).expect("spill");
        }
        let mut streak = QuarantineStreak::default();
        let mut quarantined = 0_u64;
        for _round in 0..3 {
            let (url, server) = judging_server(64);
            let outcome = drain_with_streak(
                tmp.path(),
                AuditSpillTable::PnlAudit,
                &url,
                &client(),
                &mut streak,
            )
            .await;
            drop(server);
            assert!(outcome.failed && outcome.stopped_on_refusal_streak);
            quarantined += outcome.files_quarantined;
        }
        assert_eq!(
            quarantined,
            u64::from(crate::tick_spill_replay::REPLAY_QUARANTINE_STREAK_STOP),
            "at most one streak's worth is ever set aside"
        );
        assert_eq!(
            list_spill_files(tmp.path()).len(),
            3,
            "the rest wait on disk"
        );
        // A file that lands breaks the streak; a later refusal is judged anew.
        assert!(streak_already_stopped_for(streak, 400));
        assert!(!streak_already_stopped_for(streak, 422), "another status");
        let mut fresh = streak;
        fresh.reset();
        assert!(!streak_already_stopped_for(fresh, 400));
    }

    /// Regression, B1 review 2 (2026-11-13): four single-row files QuestDB
    /// refuses one by one, then good files. Round 1 sets three aside and stops
    /// on the streak; before, every later round kept the fourth in place and
    /// stopped, so the good files behind it never drained. Now each such round
    /// probes one file past it: a probe that lands resets the streak, the good
    /// files drain in that round, and the bad file is set aside the next.
    #[tokio::test]
    async fn test_regression_refused_head_file_does_not_block_the_good_files_behind_it() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::PnlAudit, tmp.path().to_path_buf());
        for _ in 0..4 {
            spill.spill(b"pnl_audit BAD=1i 1\n", 1).expect("spill bad");
        }
        for _ in 0..3 {
            spill
                .spill(b"pnl_audit,feed=dhan x=1i 1\n", 1)
                .expect("spill good");
        }
        let mut streak = QuarantineStreak::default();
        let mut replayed = 0_u64;
        let mut quarantined = 0_u64;
        for _round in 0..3 {
            let (url, server) = judging_server(64);
            let outcome = drain_with_streak(
                tmp.path(),
                AuditSpillTable::PnlAudit,
                &url,
                &client(),
                &mut streak,
            )
            .await;
            drop(server);
            replayed += outcome.files_replayed;
            quarantined += outcome.files_quarantined;
        }
        assert_eq!(replayed, 3, "every good file drained");
        assert_eq!(quarantined, 4, "each bad file set aside once");
        assert!(list_spill_files(tmp.path()).is_empty());
    }

    /// Regression, B1 review (2026-11-13): an isolated row is counted lost
    /// ONCE, by the round that removes its file. Before, the count ran ahead
    /// of the remove, so a file that could not be removed was re-sent,
    /// re-isolated and counted lost again every round.
    #[tokio::test]
    async fn test_regression_isolated_row_is_counted_once_when_the_remove_fails() {
        let tmp = TestDir::new();
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let rows: &[u8] = b"order_audit,feed=dhan x=1i 1\norder_audit,feed=dhan BAD=1i 2\n";
        spill.spill(rows, 2).expect("spill");
        let mut isolated = 0_u64;
        for fail in [true, false] {
            FAIL_REMOVE_FOR_TEST.with(|f| f.set(fail));
            let (url, server) = judging_server(64);
            let outcome =
                drain_audit_spill_dir(tmp.path(), AuditSpillTable::OrderAudit, &url, &client())
                    .await;
            drop(server);
            assert_eq!(outcome.failed, fail, "{outcome:?}");
            isolated += outcome.rows_isolated;
        }
        FAIL_REMOVE_FOR_TEST.with(|f| f.set(false));
        assert_eq!(isolated, 1, "one refused row, counted once");
        assert!(list_spill_files(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn drain_ignores_a_fresh_tmp_file_and_quarantines_a_stale_one() {
        let tmp = TestDir::new();
        let fresh = tmp.path().join("1-1-1-1.ilp.tmp");
        std::fs::write(&fresh, ROWS).expect("fresh");
        let stale = tmp.path().join("0-1-1-1.ilp.tmp");
        std::fs::write(&stale, ROWS).expect("stale");
        let old =
            std::time::SystemTime::now() - Duration::from_secs(AUDIT_SPILL_STALE_TMP_SECS + 60);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("open")
            .set_modified(old)
            .expect("set_modified");
        let outcome = drain_audit_spill_dir(
            tmp.path(),
            AuditSpillTable::OrderAudit,
            "http://127.0.0.1:1/write",
            &client(),
        )
        .await;
        assert_eq!(
            outcome,
            AuditDrainOutcome::default(),
            "no .ilp file to send"
        );
        assert!(
            fresh.exists(),
            "a tmp file still being written is left alone"
        );
        assert!(!stale.exists());
        assert!(
            tmp.path()
                .join(QUARANTINE_DIR)
                .join("0-1-1-1.ilp.tmp")
                .exists()
        );
    }

    #[tokio::test]
    async fn drain_on_a_missing_directory_is_a_no_op() {
        let tmp = TestDir::new();
        let outcome = drain_audit_spill_dir(
            &tmp.path().join("never-created"),
            AuditSpillTable::OrderAudit,
            "http://127.0.0.1:1/write",
            &client(),
        )
        .await;
        assert_eq!(outcome, AuditDrainOutcome::default());
    }

    #[test]
    fn the_drain_never_applies_the_tick_session_window() {
        let src = include_str!("audit_spill.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or("");
        assert!(!production.contains("retain_lines_in_open_window"));
        assert!(!production.contains("replay_spill_dir("));
    }

    #[tokio::test]
    async fn spawn_audit_spill_drain_once_spawns_only_once_per_table() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Other tests in this binary may have spawned it already, so only the
        // second call's answer is fixed.
        let _ = spawn_audit_spill_drain_once(AuditSpillTable::OrderLegPnl, &cfg);
        assert!(!spawn_audit_spill_drain_once(
            AuditSpillTable::OrderLegPnl,
            &cfg
        ));
    }
}

// Kept below the tests module: `the_drain_never_applies_the_tick_session_window`
// reads the production code as the text before the first test-only attribute.

/// Removes a replayed spill file. A test can make it fail (B1 review), to
/// prove an isolated row is counted lost once even when its file must be
/// re-sent.
async fn remove_replayed_file(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_REMOVE_FOR_TEST.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("remove failure injected by a test"));
    }
    tokio::fs::remove_file(path).await
}

#[cfg(test)]
thread_local! {
    static FAIL_REMOVE_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
