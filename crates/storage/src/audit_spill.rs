//! Disk tier for the three order-side audit writers (audit PR42b).
//!
//! # Why this exists
//!
//! `order_audit`, `pnl_audit` and `order_leg_pnl` each flush one ILP batch
//! per order event. Until 2026-10-01 a failed flush DISCARDED the batch
//! (the poisoned-buffer defense), counted and coded, so a QuestDB outage
//! during the session cost every order-side audit row written in it. The
//! rows are SEBI forensics, and a counted loss is still a loss.
//!
//! Now a failed flush writes the batch's exact ILP bytes to a file under
//! `data/spill/audit/<table>/`, and one drain task per table POSTs them back
//! to QuestDB's `/write` once it answers. A row is lost only when the disk
//! tier itself refuses it (cap reached, write failed) or QuestDB permanently
//! refuses the payload; both are counted on `tv_order_audit_chain_lost_total`.
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
    QUARANTINE_DIR, REPLAY_MAX_CHUNK_BYTES, SPILL_FILE_EXTENSION, describe_send_error,
    ilp_chunk_ranges, is_permanent_refusal, list_spill_files, write_url,
};

/// Base directory of the audit disk tier. Relative, like the tick and depth
/// spill directories next to it (`data/spill/ticks`, `data/spill/depth`).
pub const AUDIT_SPILL_BASE: &str = "data/spill/audit";

/// Byte ceiling per table directory, quarantine included.
///
/// An order-side row is a few hundred bytes, so this holds well over 100,000
/// rows per table: more than a full session of order events at the vendor's
/// 7,000/day order budget, several events each.
pub const AUDIT_SPILL_MAX_BYTES_PER_TABLE: u64 = 64 * 1024 * 1024;

/// File ceiling per table directory, quarantine included.
///
/// Bounds the directory scan the cap check makes on every spill (O(files)).
pub const AUDIT_SPILL_MAX_FILES_PER_TABLE: usize = 50_000;

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

/// The three order-side audit tables that spill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSpillTable {
    /// `order_audit` (order consumer and Dhan push consumer both write it).
    OrderAudit,
    /// `pnl_audit`.
    PnlAudit,
    /// `order_leg_pnl`.
    OrderLegPnl,
}

impl AuditSpillTable {
    /// Every table, for metric seeding.
    pub const ALL: [Self; 3] = [Self::OrderAudit, Self::PnlAudit, Self::OrderLegPnl];

    /// The QuestDB table name, also the spill sub-directory name.
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        match self {
            Self::OrderAudit => crate::order_audit_persistence::ORDER_AUDIT_TABLE,
            Self::PnlAudit => crate::pnl_audit_persistence::PNL_AUDIT_TABLE,
            Self::OrderLegPnl => crate::order_leg_pnl_persistence::ORDER_LEG_PNL_TABLE,
        }
    }

    /// The `source` label this table's lost rows carry on the chain counter.
    #[must_use]
    pub const fn lost_source(self) -> &'static str {
        match self {
            Self::OrderAudit => "order_audit_spill_lost",
            Self::PnlAudit => "pnl_audit_spill_lost",
            Self::OrderLegPnl => "order_leg_pnl_spill_lost",
        }
    }

    /// The production spill directory for this table.
    #[must_use]
    pub fn default_dir(self) -> PathBuf {
        Path::new(AUDIT_SPILL_BASE).join(self.table_name())
    }

    const fn index(self) -> usize {
        match self {
            Self::OrderAudit => 0,
            Self::PnlAudit => 1,
            Self::OrderLegPnl => 2,
        }
    }
}

/// Pre-registers every audit-spill series at zero, so the first spill of a
/// session is not consumed as a delta baseline.
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
}

/// One table's spill target, held by a writer.
#[derive(Debug, Clone)]
pub struct AuditSpill {
    dir: PathBuf,
    table: AuditSpillTable,
}

impl AuditSpill {
    /// The production spill target for `table`.
    #[must_use]
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

    /// The directory this target writes to.
    #[must_use]
    pub fn dir(&self) -> &Path {
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
            .and_then(|()| std::fs::rename(&tmp_path, &final_path))
            .and_then(|()| sync_dir(&self.dir));
        if let Err(err) = written {
            // A tmp file left behind would be quarantined as stale later;
            // remove it now so the failed spill leaves nothing half-done.
            if let Err(remove_err) = std::fs::remove_file(&tmp_path) {
                tracing::debug!(%remove_err, "could not remove a failed audit spill tmp file");
            }
            return Err(err);
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

/// Bytes and file count under `dir` and its quarantine sub-directory.
fn dir_usage(dir: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut files = 0usize;
    for d in [dir.to_path_buf(), dir.join(QUARANTINE_DIR)] {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
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
    }
    (bytes, files)
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

/// The row count encoded in a spill file name, or `None` for a foreign name.
#[must_use]
pub fn rows_in_spill_file_name(path: &Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    stem.rsplit('-').next()?.parse().ok()
}

fn write_and_sync(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
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
    /// A transient failure ended the round; the file stays for next round.
    pub failed: bool,
}

/// Replays every spill file in `dir` into QuestDB, oldest first.
///
/// O(files) per round. A 2xx deletes the file. A permanent refusal (4xx other
/// than 408/429) moves it to `quarantine/`, counts its rows as lost and logs
/// AUDIT-06. Anything else ends the round with the file kept, so a struggling
/// QuestDB is not pushed harder; the next round starts from the same file.
/// No session-window filter is applied: every row is replayed as written.
pub async fn drain_audit_spill_dir(
    dir: &Path,
    table: AuditSpillTable,
    url: &str,
    client: &Client,
) -> AuditDrainOutcome {
    let name = table.table_name();
    let mut outcome = AuditDrainOutcome::default();
    quarantine_stale_tmp_files(dir, table);
    for path in list_spill_files(dir) {
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
        match post_file(&payload, url, client).await {
            PostVerdict::Accepted => {
                if let Err(err) = tokio::fs::remove_file(&path).await {
                    // Re-POSTed next round; the DEDUP key makes that harmless.
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
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                outcome.rows_replayed = outcome.rows_replayed.saturating_add(rows);
                metrics::counter!("tv_audit_spill_replayed_rows_total", "table" => name)
                    .increment(rows);
            }
            PostVerdict::Refused(status) => match quarantine(dir, &path) {
                Ok(target) => {
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
                        path = %target.display(),
                        "AUDIT-06: QuestDB permanently refused an audit spill file — \
                         {rows} row(s) set aside in quarantine (kept, not deleted)"
                    );
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
        }
    }
    outcome
}

enum PostVerdict {
    Accepted,
    Refused(u16),
    Transient,
}

async fn post_file(payload: &[u8], url: &str, client: &Client) -> PostVerdict {
    for range in ilp_chunk_ranges(payload, REPLAY_MAX_CHUNK_BYTES) {
        let chunk = payload[range].to_vec();
        match client.post(url).body(chunk).send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                let status = response.status().as_u16();
                return if is_permanent_refusal(status) {
                    PostVerdict::Refused(status)
                } else {
                    PostVerdict::Transient
                };
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
    PostVerdict::Accepted
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
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                table = table.table_name(),
                path = %path.display(),
                moved = moved.is_ok(),
                "AUDIT-06: an audit spill file was never finished (crash between write and \
                 rename) — set aside in quarantine for a manual look, not replayed"
            );
        }
    }
}

/// One flag per table, so the drain is spawned once however many writers
/// share the table.
static DRAIN_SPAWNED: [AtomicBool; 3] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

/// Spawns `table`'s drain task once per process. Later calls are no-ops.
///
/// Call it only AFTER the table's `ensure_*_table` has run: `/write` would
/// otherwise auto-create the table without its DEDUP key.
///
/// Returns `true` when this call spawned the task.
pub fn spawn_audit_spill_drain_once(table: AuditSpillTable, questdb: &QuestDbConfig) -> bool {
    if DRAIN_SPAWNED[table.index()].swap(true, Ordering::AcqRel) {
        return false;
    }
    register_audit_spill_baseline();
    let url = write_url(&questdb.host, questdb.http_port);
    let dir = table.default_dir();
    tokio::spawn(run_drain_loop(dir, table, url));
    true
}

/// Drains at once, then every [`AUDIT_SPILL_DRAIN_INTERVAL_SECS`]. Never
/// returns. A failing round is logged on its rising edge only; recovery is
/// one `info!`.
async fn run_drain_loop(dir: PathBuf, table: AuditSpillTable, url: String) {
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
    loop {
        let outcome = drain_audit_spill_dir(&dir, table, &url, &client).await;
        if outcome.failed {
            // The rows stay on disk; this counts rounds that could not finish.
            metrics::counter!("tv_audit_spill_replay_failed_total", "table" => name).increment(1);
            if !failing {
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    table = name,
                    "AUDIT-06: QuestDB is not accepting the {name} spill backlog — the rows \
                     stay on disk and are retried every {AUDIT_SPILL_DRAIN_INTERVAL_SECS}s"
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
    fn spill_refuses_past_the_byte_ceiling_and_counts_quarantine() {
        let tmp = TestDir::new();
        let q = tmp.path().join(QUARANTINE_DIR);
        std::fs::create_dir_all(&q).expect("mkdir");
        let big = std::fs::File::create(q.join("old.ilp")).expect("create");
        big.set_len(AUDIT_SPILL_MAX_BYTES_PER_TABLE)
            .expect("set_len");
        let spill = AuditSpill::in_dir(AuditSpillTable::OrderAudit, tmp.path().to_path_buf());
        let err = spill.spill(ROWS, 2).expect_err("must refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
        assert!(list_spill_files(tmp.path()).is_empty());
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
        let mut sources: Vec<&str> = AuditSpillTable::ALL
            .iter()
            .map(|t| t.lost_source())
            .collect();
        sources.dedup();
        assert_eq!(sources.len(), 3);
        let indexes: Vec<usize> = AuditSpillTable::ALL.iter().map(|t| t.index()).collect();
        assert_eq!(indexes, vec![0, 1, 2]);
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
        let (url, server) = status_server(BAD_400, 1);
        let outcome =
            drain_audit_spill_dir(tmp.path(), AuditSpillTable::OrderAudit, &url, &client()).await;
        assert_eq!(server.join().expect("join"), 1);
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
