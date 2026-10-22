//! `tickvault rebuild-box <phase> <mode> <sha> <manifest|->`: the box-side
//! half of the 2026-10-22 rebuild (plan item 66; see `rebuild_2026_10_22`).
//!
//! Sent over SSM by the runner as the one budgeted shell element in
//! `rebuild_ssm`: a copy of the deployed binary for the workflow's commit is
//! fetched to `/var/tmp` and run with this subcommand, as root. It never runs
//! from the app's own boot or from the systemd units.
//!
//! | Phase | Box | What it does |
//! |---|---|---|
//! | `copy dry` | old | inventory only: tables, partitions, rows, bytes, the keep-list's DDL, WAL segments and their verified copies, data files, disk |
//! | `copy apply` | old | stops and disables the app; waits for every table's WAL to apply; copies every WAL segment and spill/quarantine file (`raw_frame_upload`), every partition (`partition_archive`, NO drop), every keep table whole with its DDL, every other table whole, every file under `data/` but the WAL, logs and temp; rehearses the restore on a scratch table; writes the manifest; re-enables (does not start) the app |
//! | `prepare-new apply` | new | waits for first-boot provisioning; writes QuestDB's compose env for this host; starts QuestDB; restores the keep tables and the data files from the manifest, BEFORE the app's first start |
//! | `postcheck dry/apply` | new | app active, keep tables present, disk and memory as expected, Quote 34 tables reported |
//!
//! Every check that fails is a problem in the one report line it prints
//! (`REBUILD-REPORT {json}`); the runner stops on any problem. Nothing here
//! deletes data: the only drop is of the restore rehearsal's own
//! `rebuild_probe_*` scratch table on the OLD box.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::rebuild_2026_10_22::{
    BoxReport, COLD_BUCKET, CopyManifest, KeepTableCopy, MANIFEST_VERSION, Mode, Phase,
    QUOTE_34_REMOVED_TABLES, S3_PREFIX, apply_window, is_commit_sha, is_manifest_key,
    is_quote_34_removed, manifest_key,
};
use tickvault_storage::partition_archive::{PartitionArchiver, is_rebuild_safe_table_name};
use tickvault_storage::rebuild_copy::{
    QuestDbHttp, REBUILD_PROBE_TABLE_PREFIX, RebuildCold, is_safe_rel_path, rename_ddl_table,
};

/// The CLI word.
pub const REBUILD_BOX_SUBCOMMAND: &str = "rebuild-box";

/// The app's working directory on the box.
const BOX_ROOT: &str = "/opt/tickvault";
/// Its data tree, relative to [`BOX_ROOT`].
const DATA_DIR: &str = "data";
/// Top-level folders of `data/` the file copy skips: the WAL (copied segment
/// by segment, verified, by `raw_frame_upload` into `raw-frames/`), the logs
/// (shipped to CloudWatch and the S3 log export), and scratch space.
const SKIPPED_DATA_DIRS: [&str; 3] = ["ws_wal", "logs", "tmp"];
/// QuestDB's HTTP port, published on the host by `docker-compose.yml`.
const QUESTDB_HTTP_PORT: u16 = 9000;
/// QuestDB pg-wire and ILP ports (compose defaults; the archiver's audit
/// writer uses ILP for its forensic rows).
const QUESTDB_PG_PORT: u16 = 8812;
const QUESTDB_ILP_PORT: u16 = 9009;
/// How long to wait for every table's WAL to apply after the app stops.
const WAL_APPLY_WAIT_SECS: u64 = 1800;
/// How long to wait for QuestDB to answer on the new box.
const QUESTDB_READY_WAIT_SECS: u64 = 600;
/// How long to wait for a restored table's rows to become visible.
const RESTORE_VISIBLE_WAIT_SECS: u64 = 600;
/// How long the postcheck waits for the app to become active.
const APP_ACTIVE_WAIT_SECS: u64 = 600;
/// How long `prepare-new` waits for first-boot provisioning to finish.
const CLOUD_INIT_WAIT_SECS: u64 = 1800;
/// Written by cloud-init when its last stage ends. Read as files, never by
/// spawning `cloud-init` (a Python program; the spawn allowlist refuses it).
const CLOUD_INIT_DONE: &str = "/var/lib/cloud/instance/boot-finished";
const CLOUD_INIT_RESULT: &str = "/run/cloud-init/result.json";
/// Poll interval for every wait above.
const POLL_SECS: u64 = 5;
/// Raw-frame upload passes before giving up on a backlog.
const MAX_UPLOAD_PASSES: usize = 6;
/// The restore rehearsal prefers this table (the SEBI single source of truth).
const REHEARSAL_TABLE: &str = "instrument_lifecycle";
/// Where the box keeps its full local report.
const LOCAL_REPORT_DIR: &str = "/var/tmp";
/// The new root is 150 GB; a filesystem smaller than this did not follow it.
const NEW_ROOT_MIN_FS_BYTES: u64 = 140 * 1_000_000_000;
/// The new root is 150 GB; a filesystem larger than this is not the new root.
const NEW_ROOT_MAX_FS_BYTES: u64 = 170 * 1_000_000_000;

/// True when the process was started as `tickvault rebuild-box ...`.
#[must_use]
pub fn is_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(REBUILD_BOX_SUBCOMMAND)
}

/// One line to stdout (the SSM output). A failed write has nowhere to go.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "rebuild-box: {line}").unwrap_or_default();
}

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxArgs {
    /// Box phase.
    pub phase: Phase,
    /// Dry or apply.
    pub mode: Mode,
    /// The commit whose binary this is.
    pub sha: String,
    /// The copy manifest (prepare-new, postcheck).
    pub manifest: Option<String>,
}

/// Parses `rebuild-box <phase> <mode> <sha> <manifest|->`.
///
/// # Errors
/// The first argument that fails validation.
pub fn parse_args(args: &[String]) -> Result<BoxArgs, String> {
    let get = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    let phase = Phase::parse(get(2))
        .filter(|p| p.runs_on_box())
        .ok_or_else(|| format!("phase {:?} does not run on the box", get(2)))?;
    let mode = Mode::parse(get(3)).ok_or_else(|| format!("mode {:?}", get(3)))?;
    let sha = get(4);
    if !is_commit_sha(sha) {
        return Err(format!("commit {sha:?}"));
    }
    let manifest = crate::rebuild_ssm::manifest_arg(get(5)).map(str::to_string);
    if let Some(m) = &manifest
        && !is_manifest_key(m)
    {
        return Err(format!("manifest key {m:?}"));
    }
    if phase == Phase::PrepareNew && manifest.is_none() {
        return Err(String::from("prepare-new needs the copy manifest"));
    }
    if phase == Phase::PrepareNew && mode == Mode::Dry {
        return Err(String::from(
            "prepare-new has no dry run (the new box holds nothing to inventory)",
        ));
    }
    Ok(BoxArgs {
        phase,
        mode,
        sha: sha.to_string(),
        manifest,
    })
}

/// Runs the subcommand; the process exit code (0 only when the report is ok).
// TEST-EXEMPT: process entry point on the box; the decisions are the tested pure functions it calls
pub async fn run(args: &[String]) -> i32 {
    let report = match parse_args(args) {
        Err(why) => {
            let mut r = BoxReport::new(Phase::Copy, Mode::Dry);
            r.problem(format!("bad arguments: {why}"));
            r
        }
        Ok(parsed) => {
            emit(&format!(
                "{} {} starting (binary of commit {})",
                parsed.phase.as_str(),
                parsed.mode.as_str(),
                parsed.sha
            ));
            if let Err(err) = std::env::set_current_dir(BOX_ROOT) {
                let mut r = BoxReport::new(parsed.phase, parsed.mode);
                r.problem(format!("cannot enter {BOX_ROOT}: {err}"));
                r
            } else {
                match (parsed.phase, parsed.mode) {
                    (Phase::Copy, Mode::Dry) => inventory().await,
                    (Phase::Copy, Mode::Apply) => copy_apply().await,
                    (Phase::PrepareNew, _) => {
                        prepare_new(parsed.manifest.as_deref().unwrap_or_default()).await
                    }
                    (Phase::Postcheck, mode) => postcheck(mode, parsed.manifest.as_deref()).await,
                    (phase, mode) => {
                        let mut r = BoxReport::new(phase, mode);
                        r.problem("no box step for this phase");
                        r
                    }
                }
            }
        }
    };
    let (report, line) = report.finish();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let local = PathBuf::from(LOCAL_REPORT_DIR).join(format!(
        "tv-rebuild-{}-{}-{stamp}.json",
        report.phase, report.mode
    ));
    if let Ok(full) = serde_json::to_vec_pretty(&report) {
        std::fs::write(&local, full).ok(); // the report line below is the record that matters
    }
    {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").unwrap_or_default();
    }
    i32::from(!report.ok)
}

// ---------------------------------------------------------------------------
// Pure helpers (tested)
// ---------------------------------------------------------------------------

/// How the copy treats one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablePlan {
    /// SEBI/audit keep-list: whole table + DDL, restored onto the new disk.
    Keep,
    /// Time-partitioned on `ts`: every partition copied at the archiver's key.
    Partitions,
    /// No time partitions (or not on `ts`): copied whole, not restored.
    Whole,
    /// QuestDB's own tables (`telemetry*`, `sys.*`): not our data, skipped.
    Internal,
    /// A name the copy refuses to put in SQL: a problem, never skipped quietly.
    Unsafe,
}

/// Classifies a table from its `tables()` row.
#[must_use]
pub fn plan_table(name: &str, partition_by: &str, designated: &str, keep: &[&str]) -> TablePlan {
    if name.starts_with("sys.") || name.starts_with("telemetry") {
        return TablePlan::Internal;
    }
    if !is_rebuild_safe_table_name(name) {
        return TablePlan::Unsafe;
    }
    // A Quote 34 table on the keep list (the legacy `top_volume_rank`) is
    // still copied to S3, but by partition or whole, never as a keep table,
    // so the restore cannot re-create it on the new disk.
    if keep.contains(&name) && !is_quote_34_removed(name) {
        return TablePlan::Keep;
    }
    if partition_by != "NONE" && !partition_by.is_empty() && designated == "ts" {
        TablePlan::Partitions
    } else {
        TablePlan::Whole
    }
}

/// `true` for a WAL segment file name (`ws-frames-<digits>.wal`).
#[must_use]
pub fn is_wal_segment_name(name: &str) -> bool {
    name.strip_prefix("ws-frames-")
        .and_then(|r| r.strip_suffix(".wal"))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The `wal_tables()` rows that are not fully applied, by column name:
/// suspended, or `writerTxn != sequencerTxn`. `None` when the body does not
/// parse (which the caller treats as "not proven applied").
#[must_use]
pub fn unapplied_wal_tables(json: &serde_json::Value) -> Option<Vec<String>> {
    let columns = json.get("columns")?.as_array()?;
    let idx = |want: &str| {
        columns
            .iter()
            .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(want))
    };
    let (n, s, w, q) = (
        idx("name")?,
        idx("suspended")?,
        idx("writerTxn")?,
        idx("sequencerTxn")?,
    );
    let mut out = Vec::new();
    for row in json.get("dataset")?.as_array()? {
        let cells = row.as_array()?;
        let name = cells.get(n)?.as_str()?.to_string();
        let suspended = cells.get(s)?.as_bool()?;
        let (wt, st) = (cells.get(w)?.as_i64()?, cells.get(q)?.as_i64()?);
        if suspended || wt != st {
            out.push(name);
        }
    }
    Some(out)
}

/// One line from cloud-init's `result.json` (`{"v1":{"errors":[..]}}`):
/// `done` or `done with N error(s): first`. Reported, never trusted.
fn cloud_init_summary(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "done, result.json did not parse".to_string();
    };
    match v.pointer("/v1/errors").and_then(|e| e.as_array()) {
        Some(errs) if errs.is_empty() => "done".to_string(),
        Some(errs) => format!(
            "done with {} error(s): {}",
            errs.len(),
            errs[0].as_str().unwrap_or("(not text)")
        ),
        None => "done, result.json has no v1.errors".to_string(),
    }
}

/// `(name, partitionBy, designatedTimestamp)` rows of `tables()`, by column
/// name. A row whose designated timestamp is null reads as an empty string.
#[must_use]
pub fn parse_tables(json: &serde_json::Value) -> Option<Vec<(String, String, String)>> {
    let columns = json.get("columns")?.as_array()?;
    let idx = |want: &str| {
        columns
            .iter()
            .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(want))
    };
    let (n, p, d) = (
        idx("table_name")?,
        idx("partitionBy")?,
        idx("designatedTimestamp")?,
    );
    let mut out = Vec::new();
    for row in json.get("dataset")?.as_array()? {
        let cells = row.as_array()?;
        let s = |i: usize| {
            cells
                .get(i)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        out.push((s(n), s(p), s(d)));
    }
    out.sort();
    Some(out)
}

// ---------------------------------------------------------------------------
// Host helpers
// ---------------------------------------------------------------------------

/// `systemctl <args>`; `(succeeded, trimmed stdout)`.
fn systemctl(args: &[&str]) -> (bool, String) {
    match Command::new("systemctl").args(args).output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ),
        Err(err) => (false, format!("systemctl could not start: {err}")),
    }
}

/// The QuestDB client on the host's published port.
fn questdb() -> Result<QuestDbHttp, String> {
    // Host-side tool: QuestDB publishes 9000 on the host's loopback (the
    // compose default bind); there is no Docker DNS name outside a container.
    QuestDbHttp::new(
        &std::net::Ipv4Addr::LOCALHOST.to_string(),
        QUESTDB_HTTP_PORT,
    )
    .map_err(|e| format!("{e:#}"))
}

/// The archiver on the host's published port, against the cold bucket.
async fn archiver() -> Result<PartitionArchiver, String> {
    let qdb = tickvault_common::config::QuestDbConfig {
        host: std::net::Ipv4Addr::LOCALHOST.to_string(),
        http_port: QUESTDB_HTTP_PORT,
        pg_port: QUESTDB_PG_PORT,
        ilp_port: QUESTDB_ILP_PORT,
    };
    PartitionArchiver::new_for_rebuild(&qdb, COLD_BUCKET)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// `MemTotal` in GiB, and the online core count.
fn host_size() -> (u64, usize) {
    let mem_g = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines().find_map(|l| {
                l.strip_prefix("MemTotal:")?
                    .trim()
                    .strip_suffix(" kB")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
        })
        .map_or(0, |kib| kib / 1_048_576);
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    (mem_g, cores)
}

/// `(total, free)` bytes of the filesystem holding `/`.
fn root_fs() -> Option<(u64, u64)> {
    match tickvault_storage::disk_health_watcher::probe_disk_free_bytes(Path::new("/")) {
        tickvault_storage::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } => Some((total_bytes, free_bytes)),
        tickvault_storage::disk_health_watcher::DiskHealthOutcome::ProbeFailed { .. } => None,
    }
}

/// Every regular file under `data/` but [`SKIPPED_DATA_DIRS`], as
/// `(relative path, bytes)`, sorted. Symlinks and other non-files are
/// returned separately so the report can name them.
fn data_files() -> (Vec<(String, u64)>, Vec<String>) {
    let mut files = Vec::new();
    let mut other = Vec::new();
    let root = Path::new(DATA_DIR);
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            other.push(format!("{} (unreadable)", dir.display()));
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().to_string();
            if dir == root && SKIPPED_DATA_DIRS.contains(&rel.as_str()) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                other.push(format!("{rel} (unreadable)"));
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                files.push((rel, meta.len()));
            } else {
                other.push(format!("{rel} (not a regular file)"));
            }
        }
    }
    files.sort();
    other.sort();
    (files, other)
}

/// WAL segments under the WAL root, `replaying/` and `archive/`, as
/// `(path, bytes, has a verified copy)`.
fn wal_segments() -> Vec<(PathBuf, u64, bool)> {
    let wal = crate::boot_helpers::ws_wal_dir();
    let markers = tickvault_storage::raw_frame_upload::markers_dir(&wal);
    let mut out = Vec::new();
    for dir in [wal.clone(), wal.join("replaying"), wal.join("archive")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_wal_segment_name(&name) {
                continue;
            }
            let len = entry.metadata().map_or(0, |m| m.len());
            let marked = tickvault_storage::raw_frame_upload::marker_matches(&markers, &path, len);
            out.push((path, len, marked));
        }
    }
    out.sort();
    out
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
}

// ---------------------------------------------------------------------------
// copy dry: the inventory
// ---------------------------------------------------------------------------

async fn tables_of(q: &QuestDbHttp) -> Result<Vec<(String, String, String)>, String> {
    let v = q
        .exec("SELECT table_name, partitionBy, designatedTimestamp FROM tables()")
        .await?;
    parse_tables(&v).ok_or_else(|| String::from("tables() did not parse"))
}

async fn inventory() -> BoxReport {
    let mut r = BoxReport::new(Phase::Copy, Mode::Dry);
    let (_, state) = systemctl(&["is-active", "tickvault"]);
    r.line(format!(
        "app tickvault.service: {state} (copy apply stops and disables it first)"
    ));
    let (mem_g, cores) = host_size();
    r.line(format!("host: {cores} cores, {mem_g} GiB"));
    if let Some((total, free)) = root_fs() {
        r.line(format!(
            "root filesystem: {} total, {} free",
            gib(total),
            gib(free)
        ));
    }
    let q = match questdb() {
        Ok(q) => q,
        Err(e) => {
            r.problem(e);
            return r;
        }
    };
    let tables = match tables_of(&q).await {
        Ok(t) => t,
        Err(e) => {
            r.problem(format!("QuestDB tables(): {e}"));
            return r;
        }
    };
    let arch = match archiver().await {
        Ok(a) => a,
        Err(e) => {
            r.problem(format!("archiver: {e}"));
            return r;
        }
    };
    let keep = tickvault_storage::partition_manager::rebuild_keep_tables();
    let mut detail: Vec<String> = Vec::new();
    let (mut parts_total, mut rows_total, mut bytes_total, mut present_total) =
        (0u64, 0u64, 0u64, 0u64);
    for (name, partition_by, designated) in &tables {
        match plan_table(name, partition_by, designated, &keep) {
            TablePlan::Internal => r.line(format!("skip {name}: QuestDB internal table")),
            TablePlan::Unsafe => r.problem(format!("table name {name:?} cannot be copied safely")),
            TablePlan::Keep => match q.show_create_table(name).await {
                Ok(_) => {
                    let rows = q.count(name).await.unwrap_or(0);
                    r.line(format!("keep {name}: {rows} rows — copied whole with its DDL, restored on the new disk"));
                }
                Err(e) => r.problem(format!("keep {name}: SHOW CREATE TABLE failed: {e}")),
            },
            TablePlan::Whole => {
                let rows = q.count(name).await.unwrap_or(0);
                r.line(format!("whole {name}: {rows} rows ({partition_by}, ts={designated:?}) — copied whole, not restored"));
            }
            TablePlan::Partitions => match arch.rebuild_list_partitions(name).await {
                Ok(listing) => {
                    let (mut n, mut rows, mut bytes, mut present) = (0u64, 0u64, 0u64, 0u64);
                    for p in &listing {
                        match p {
                            Ok(p) => {
                                n += 1;
                                rows += p.num_rows;
                                bytes += p.disk_bytes;
                                let len = arch.rebuild_canonical_len(name, &p.name).await;
                                if len.is_some() {
                                    present += 1;
                                }
                                detail.push(format!(
                                    "{name} {} rows={} disk={} active={} s3={}",
                                    p.name,
                                    p.num_rows,
                                    p.disk_bytes,
                                    p.active,
                                    len.map_or_else(
                                        || String::from("absent"),
                                        |l| format!("{l} B")
                                    )
                                ));
                            }
                            Err(bad) => r.problem(format!(
                                "{name}: partition {bad:?} cannot be copied safely"
                            )),
                        }
                    }
                    parts_total += n;
                    rows_total += rows;
                    bytes_total += bytes;
                    present_total += present;
                    r.line(format!(
                        "partitions {name}: {n} ({rows} rows, {} on disk), {present} already have a canonical S3 object (re-exported and compared anyway)",
                        gib(bytes)
                    ));
                }
                Err(e) => r.problem(format!("{name}: partition listing failed: {e:#}")),
            },
        }
    }
    r.line(format!(
        "partitions total: {parts_total} ({rows_total} rows, {} on disk), {present_total} with a canonical S3 object",
        gib(bytes_total)
    ));
    let segs = wal_segments();
    let seg_bytes: u64 = segs.iter().map(|s| s.1).sum();
    let unmarked = segs.iter().filter(|s| !s.2).count();
    r.line(format!(
        "WAL: {} segments ({}), {unmarked} without a verified copy in raw-frames/ (copy apply uploads them)",
        segs.len(),
        gib(seg_bytes)
    ));
    let (files, other) = data_files();
    let file_bytes: u64 = files.iter().map(|f| f.1).sum();
    r.line(format!(
        "data files to copy and restore (data/ minus ws_wal, logs, tmp): {} files, {}",
        files.len(),
        gib(file_bytes)
    ));
    for (rel, _) in &files {
        if !is_safe_rel_path(rel) {
            r.problem(format!("data file {rel:?} has a name the copy refuses"));
        }
    }
    for o in other {
        r.line(format!("not copied: {o}"));
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let detail_path =
        PathBuf::from(LOCAL_REPORT_DIR).join(format!("tv-rebuild-inventory-{stamp}.txt"));
    if std::fs::write(&detail_path, detail.join("\n")).is_ok() {
        r.line(format!("per-partition detail: {}", detail_path.display()));
    }
    r
}

// ---------------------------------------------------------------------------
// copy apply
// ---------------------------------------------------------------------------

async fn copy_apply() -> BoxReport {
    let mut r = BoxReport::new(Phase::Copy, Mode::Apply);
    if let Err(why) = apply_window(chrono::Utc::now()) {
        r.problem(format!("refused: {why}"));
        return r;
    }
    let (was_enabled, _) = systemctl(&["is-enabled", "--quiet", "tickvault"]);
    let (stopped, out) = systemctl(&["stop", "tickvault"]);
    if !stopped {
        r.problem(format!("could not stop tickvault.service: {out}"));
        return r;
    }
    // Disabled for the copy so nothing restarts it; enabled again (never
    // started) on every path out, so the old disk still boots the app if
    // the rebuild is rolled back.
    let (disabled, out) = systemctl(&["disable", "tickvault"]);
    if disabled {
        r.line("app stopped and disabled for the copy");
    } else {
        // The copy still runs (the app is stopped); a reboot in the middle
        // could restart it, which the WAL check would then catch.
        r.line(format!(
            "app stopped; disable failed ({out}), so a reboot would restart it"
        ));
    }
    copy_body(&mut r).await;
    if was_enabled {
        let (ok, out) = systemctl(&["enable", "tickvault"]);
        if ok {
            r.line("app unit enabled again (not started)");
        } else {
            r.problem(format!("could not re-enable tickvault.service: {out}"));
        }
    }
    r
}

async fn copy_body(r: &mut BoxReport) {
    let q = match questdb() {
        Ok(q) => q,
        Err(e) => return r.problem(e),
    };
    // 1. Every table's WAL applied (nothing acknowledged is left unwritten).
    let deadline = Instant::now() + Duration::from_secs(WAL_APPLY_WAIT_SECS);
    loop {
        let verdict = match q
            .exec("SELECT name, suspended, writerTxn, sequencerTxn FROM wal_tables()")
            .await
        {
            Ok(v) => unapplied_wal_tables(&v),
            Err(_) => None,
        };
        match verdict {
            Some(v) if v.is_empty() => {
                r.line("every table's WAL is applied");
                break;
            }
            other => {
                if Instant::now() >= deadline {
                    return r.problem(format!(
                        "WAL not applied after {WAL_APPLY_WAIT_SECS} s: {other:?}"
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
    // 2. Raw WAL segments and the spill / quarantine files, verified.
    let Some(store) = tickvault_storage::s3_cold::S3Cold::load(COLD_BUCKET).await else {
        return r.problem("cold bucket did not resolve");
    };
    let wal = crate::boot_helpers::ws_wal_dir();
    let always = || true;
    for pass in 0..MAX_UPLOAD_PASSES {
        let summary =
            tickvault_storage::raw_frame_upload::run_pass(&store, &wal, usize::MAX, &always).await;
        let today =
            tickvault_storage::seal_spill::live_spill_file_name(chrono::Utc::now().timestamp());
        let files = tickvault_storage::raw_frame_upload::run_spill_and_quarantine_passes(
            &store,
            Path::new("data/spill"),
            Path::new(tickvault_storage::tick_persistence::TICK_SPILL_DIR),
            Path::new(tickvault_storage::depth_persistence::DEPTH_SPILL_DIR),
            Path::new(tickvault_storage::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR),
            &today,
            usize::MAX,
            &always,
        )
        .await;
        r.line(format!(
            "raw upload pass {pass}: WAL marked {} backlog {} failed {}; files marked {} backlog {} failed {}",
            summary.marked(),
            summary.backlog_after,
            summary.failed,
            files.marked(),
            files.backlog_after,
            files.failed
        ));
        if summary.backlog_after == 0 && files.backlog_after == 0 {
            break;
        }
    }
    let segs = wal_segments();
    let unmarked: Vec<_> = segs.iter().filter(|s| !s.2).collect();
    if !unmarked.is_empty() {
        r.problem(format!(
            "{} WAL segments still have no verified copy in raw-frames/ (first: {})",
            unmarked.len(),
            unmarked[0].0.display()
        ));
    }
    let mut manifest = CopyManifest {
        version: MANIFEST_VERSION,
        wal_segments_verified: (segs.len() - unmarked.len()) as u64,
        ..CopyManifest::default()
    };
    // 3. Tables.
    let tables = match tables_of(&q).await {
        Ok(t) => t,
        Err(e) => return r.problem(format!("QuestDB tables(): {e}")),
    };
    let mut arch = match archiver().await {
        Ok(a) => a,
        Err(e) => return r.problem(format!("archiver: {e}")),
    };
    let keep = tickvault_storage::partition_manager::rebuild_keep_tables();
    let tables_prefix = format!("{S3_PREFIX}/tables");
    let keep_prefix = format!("{S3_PREFIX}/keep");
    for (name, partition_by, designated) in &tables {
        match plan_table(name, partition_by, designated, &keep) {
            TablePlan::Internal => {}
            TablePlan::Unsafe => r.problem(format!("table name {name:?} cannot be copied safely")),
            TablePlan::Partitions => {
                let listing = match arch.rebuild_list_partitions(name).await {
                    Ok(l) => l,
                    Err(e) => {
                        r.problem(format!("{name}: partition listing failed: {e:#}"));
                        continue;
                    }
                };
                let (mut ok, mut rows) = (0u64, 0u64);
                for p in listing {
                    match p {
                        Err(bad) => {
                            r.problem(format!("{name}: partition {bad:?} cannot be copied safely"))
                        }
                        Ok(p) => match arch.rebuild_copy_partition(name, &p.name).await {
                            Ok(c) => {
                                ok += 1;
                                rows += c.rows;
                                manifest.partitions.push(c);
                            }
                            Err(e) => r.problem(format!("{name} {}: {e}", p.name)),
                        },
                    }
                }
                r.line(format!(
                    "copied {name}: {ok} partitions, {rows} rows, verified"
                ));
            }
            TablePlan::Whole => match arch.rebuild_copy_whole_table(name, &tables_prefix).await {
                Ok(c) => {
                    r.line(format!("copied {name} whole: {} rows, verified", c.rows));
                    manifest.whole_tables.push(c);
                }
                Err(e) => r.problem(format!("{name}: {e}")),
            },
            TablePlan::Keep => {
                let ddl = match q.show_create_table(name).await {
                    Ok(d) => d,
                    Err(e) => {
                        r.problem(format!("keep {name}: {e}"));
                        continue;
                    }
                };
                match arch.rebuild_copy_whole_table(name, &keep_prefix).await {
                    Ok(c) => {
                        r.line(format!(
                            "copied keep {name}: {} rows with DDL, verified",
                            c.rows
                        ));
                        manifest.keep_tables.push(KeepTableCopy { ddl, data: c });
                    }
                    Err(e) => r.problem(format!("keep {name}: {e}")),
                }
            }
        }
    }
    // 4. Data files.
    let cold = RebuildCold::connect(COLD_BUCKET).await;
    let (files, other) = data_files();
    let data_root = Path::new(DATA_DIR);
    let (mut n, mut bytes) = (0u64, 0u64);
    for (rel, _) in &files {
        match cold.copy_file(data_root, rel).await {
            Ok(c) => {
                n += 1;
                bytes += c.bytes;
                manifest.files.push(c);
            }
            Err(e) => r.problem(format!("data/{rel}: {e}")),
        }
    }
    r.line(format!("copied {n} data files ({}), verified", gib(bytes)));
    for o in other {
        r.line(format!("not copied: {o}"));
    }
    // 5. Restore rehearsal on a scratch table, so the restore path is proven
    //    tonight on the old box before the disk changes.
    rehearse_restore(r, &q, &cold, &manifest).await;
    // 6. The manifest, to a new key.
    let now = chrono::Utc::now();
    manifest.created_utc = now.to_rfc3339();
    let key = manifest_key(now);
    match serde_json::to_vec(&manifest) {
        Ok(body) => match cold.put_new_json(&key, body).await {
            Ok(()) => {
                r.line(format!("manifest written: s3://{COLD_BUCKET}/{key}"));
                r.manifest_key = Some(key);
            }
            Err(e) => r.problem(format!("manifest upload failed: {e}")),
        },
        Err(e) => r.problem(format!("manifest did not serialise: {e}")),
    }
}

async fn rehearse_restore(
    r: &mut BoxReport,
    q: &QuestDbHttp,
    cold: &RebuildCold,
    manifest: &CopyManifest,
) {
    let pick = manifest
        .keep_tables
        .iter()
        .find(|k| k.data.table == REHEARSAL_TABLE && k.data.rows > 0)
        .or_else(|| manifest.keep_tables.iter().find(|k| k.data.rows > 0));
    let Some(keep) = pick else {
        return r.problem("restore rehearsal: no keep table with rows to rehearse on");
    };
    let probe = format!("{REBUILD_PROBE_TABLE_PREFIX}{}", keep.data.table);
    let Some(ddl) = rename_ddl_table(&keep.ddl, &keep.data.table, &probe) else {
        return r.problem(format!(
            "restore rehearsal: DDL of {} did not rename",
            keep.data.table
        ));
    };
    // A scratch table left by an earlier rehearsal, if any; absent is fine.
    q.drop_probe_table(&probe).await.ok();
    let outcome = async {
        q.exec(&ddl).await?;
        let path = PathBuf::from("data/tmp").join(format!("{probe}.csv.gz"));
        std::fs::create_dir_all("data/tmp").ok(); // the write that follows reports a failure
        std::fs::remove_file(&path).ok(); // absent is fine
        cold.fetch_file(
            &keep.data.s3_key,
            &path,
            keep.data.gzip_bytes,
            &keep.data.gzip_sha256_hex,
        )
        .await?;
        let imported = q.import_csv_gz(&probe, &path).await;
        std::fs::remove_file(&path).ok(); // absent is fine
        let imported = imported?;
        let visible = wait_count(q, &probe, keep.data.rows).await;
        Ok::<(u64, u64), String>((imported, visible))
    }
    .await;
    let dropped = q.drop_probe_table(&probe).await;
    match outcome {
        Ok((imported, visible)) if imported == keep.data.rows && visible == keep.data.rows => r.line(
            format!(
                "restore rehearsal: {} rows of {} re-created from DDL and imported into {probe}, counts match",
                keep.data.rows, keep.data.table
            ),
        ),
        Ok((imported, visible)) => r.problem(format!(
            "restore rehearsal on {}: {imported} imported, {visible} visible, {} expected",
            keep.data.table, keep.data.rows
        )),
        Err(e) => r.problem(format!("restore rehearsal on {}: {e}", keep.data.table)),
    }
    if let Err(e) = dropped {
        r.line(format!("restore rehearsal: scratch table {probe} not dropped ({e}); harmless, on the old disk only"));
    }
}

/// Polls `count()` until it reaches `want` (the WAL applies the import
/// asynchronously) or the wait ends; the last count read.
async fn wait_count(q: &QuestDbHttp, table: &str, want: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(RESTORE_VISIBLE_WAIT_SECS);
    loop {
        let n = q.count(table).await.unwrap_or(0);
        if n >= want || Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
}

// ---------------------------------------------------------------------------
// prepare-new apply (the new root, before the app's first start)
// ---------------------------------------------------------------------------

async fn prepare_new(manifest_key: &str) -> BoxReport {
    let mut r = BoxReport::new(Phase::PrepareNew, Mode::Apply);
    if let Err(why) = apply_window(chrono::Utc::now()) {
        r.problem(format!("refused: {why}"));
        return r;
    }
    // First-boot provisioning (user-data) runs once on a fresh root. Its
    // exit status is reported, not trusted: on a 2-vCPU host its own
    // `docker compose up` can fail on the compose cpuset default, which this
    // phase fixes below.
    let deadline = Instant::now() + Duration::from_secs(CLOUD_INIT_WAIT_SECS);
    while !Path::new(CLOUD_INIT_DONE).exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
    let cloud = if Path::new(CLOUD_INIT_DONE).exists() {
        match std::fs::read(CLOUD_INIT_RESULT) {
            Ok(body) => cloud_init_summary(&body),
            Err(e) => format!("done, {CLOUD_INIT_RESULT} unreadable: {e}"),
        }
    } else {
        format!("not finished after {CLOUD_INIT_WAIT_SECS} s")
    };
    r.line(format!("first-boot provisioning: {cloud}"));
    if !Path::new("/opt/tickvault/repo/deploy/docker/docker-compose.yml").exists() {
        r.problem("the repo clone from first boot is missing; QuestDB cannot be started");
        return r;
    }
    let (mem_g, cores) = host_size();
    r.line(format!("host: {cores} cores, {mem_g} GiB"));
    r.line(crate::host_tuning::apply_questdb_host_env(true));
    let code = crate::ensure_questdb::run().await;
    r.line(format!("ensure-questdb exit {code}"));
    let q = match questdb() {
        Ok(q) => q,
        Err(e) => {
            r.problem(e);
            return r;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(QUESTDB_READY_WAIT_SECS);
    while q.exec("SELECT 1").await.is_err() {
        if Instant::now() >= deadline {
            r.problem(format!(
                "QuestDB did not answer within {QUESTDB_READY_WAIT_SECS} s"
            ));
            return r;
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
    r.line("QuestDB answers");
    let cold = RebuildCold::connect(COLD_BUCKET).await;
    std::fs::create_dir_all("data/tmp").ok(); // the write that follows reports a failure
    let manifest: CopyManifest = match cold.get_small(manifest_key, Path::new("data/tmp")).await {
        Ok(body) => match serde_json::from_slice(&body) {
            Ok(m) => m,
            Err(e) => {
                r.problem(format!("manifest did not parse: {e}"));
                return r;
            }
        },
        Err(e) => {
            r.problem(format!("manifest download failed: {e}"));
            return r;
        }
    };
    if manifest.version != MANIFEST_VERSION {
        r.problem(format!(
            "manifest version {} (expected {MANIFEST_VERSION})",
            manifest.version
        ));
        return r;
    }
    restore_tables(&mut r, &q, &cold, &manifest).await;
    restore_files(&mut r, &cold, &manifest).await;
    r
}

async fn restore_tables(
    r: &mut BoxReport,
    q: &QuestDbHttp,
    cold: &RebuildCold,
    manifest: &CopyManifest,
) {
    let existing = match q.table_names().await {
        Ok(t) => t,
        Err(e) => return r.problem(format!("tables(): {e}")),
    };
    for keep in &manifest.keep_tables {
        let t = keep.data.table.as_str();
        if is_quote_34_removed(t) {
            // Never in a manifest this build writes; refused here as well so
            // no manifest can re-create a Quote 34 table.
            r.line(format!("restore {t}: skipped, removed by Quote 34"));
            continue;
        }
        if existing.iter().any(|e| e == t) {
            match q.count(t).await {
                Ok(n) if n == keep.data.rows => {
                    r.line(format!("restore {t}: already holds {n} rows"));
                    continue;
                }
                Ok(0) => {}
                Ok(n) if keep.ddl.contains("DEDUP UPSERT KEYS") => {
                    r.line(format!("restore {t}: holds {n} of {} rows; re-importing (DEDUP keys make it converge)", keep.data.rows));
                }
                Ok(n) => {
                    r.problem(format!(
                        "restore {t}: holds {n} of {} rows and has no DEDUP keys; left as it is",
                        keep.data.rows
                    ));
                    continue;
                }
                Err(e) => {
                    r.problem(format!("restore {t}: {e}"));
                    continue;
                }
            }
        } else if let Err(e) = q.exec(&keep.ddl).await {
            r.problem(format!("restore {t}: DDL failed: {e}"));
            continue;
        }
        let path = PathBuf::from("data/tmp").join(format!("restore-{t}.csv.gz"));
        std::fs::remove_file(&path).ok(); // absent is fine
        if let Err(e) = cold
            .fetch_file(
                &keep.data.s3_key,
                &path,
                keep.data.gzip_bytes,
                &keep.data.gzip_sha256_hex,
            )
            .await
        {
            r.problem(format!("restore {t}: download failed: {e}"));
            continue;
        }
        let imported = q.import_csv_gz(t, &path).await;
        std::fs::remove_file(&path).ok(); // absent is fine
        match imported {
            Ok(n) => {
                let visible = wait_count(q, t, keep.data.rows).await;
                if visible == keep.data.rows {
                    r.line(format!(
                        "restore {t}: {n} rows imported, {visible} visible — matches the copy"
                    ));
                } else {
                    r.problem(format!(
                        "restore {t}: {visible} visible, {} expected",
                        keep.data.rows
                    ));
                }
            }
            Err(e) => r.problem(format!("restore {t}: import failed: {e}")),
        }
    }
}

async fn restore_files(r: &mut BoxReport, cold: &RebuildCold, manifest: &CopyManifest) {
    let data_root = Path::new(DATA_DIR);
    // The app runs as the owner of `data/` (ec2-user, set by first boot);
    // restored files are handed to that owner so the app can rename and
    // append them.
    let owner = std::fs::metadata(data_root).ok().map(|m| {
        use std::os::unix::fs::MetadataExt as _;
        (m.uid(), m.gid())
    });
    let (mut n, mut failed) = (0u64, 0u64);
    for f in &manifest.files {
        if !is_safe_rel_path(&f.rel_path) {
            r.problem(format!("manifest file {:?} has an unsafe path", f.rel_path));
            failed += 1;
            continue;
        }
        let dest = data_root.join(&f.rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok(); // the fetch that follows reports a failure
        }
        match cold
            .fetch_file(&f.s3_key, &dest, f.bytes, &f.sha256_hex)
            .await
        {
            Ok(()) => {
                n += 1;
                if let Some((uid, gid)) = owner {
                    let mut p = Some(dest.as_path());
                    while let Some(path) = p {
                        if path == data_root {
                            break;
                        }
                        if let Err(e) = std::os::unix::fs::chown(path, Some(uid), Some(gid)) {
                            // The app runs as the data folder's owner and
                            // could not write a root-owned file back.
                            r.problem(format!("chown {}: {e}", path.display()));
                        }
                        p = path.parent();
                    }
                }
            }
            Err(e) => {
                failed += 1;
                r.problem(format!("restore data/{}: {e}", f.rel_path));
            }
        }
    }
    r.line(format!("restored {n} data files ({failed} failed)"));
}

// ---------------------------------------------------------------------------
// postcheck
// ---------------------------------------------------------------------------

async fn postcheck(mode: Mode, manifest_key: Option<&str>) -> BoxReport {
    let mut r = BoxReport::new(Phase::Postcheck, mode);
    let deadline = Instant::now() + Duration::from_secs(APP_ACTIVE_WAIT_SECS);
    loop {
        let (_, state) = systemctl(&["is-active", "tickvault"]);
        if state == "active" {
            r.line("app tickvault.service: active");
            break;
        }
        if mode == Mode::Dry || Instant::now() >= deadline {
            r.problem(format!("app tickvault.service is {state}"));
            break;
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
    let (mem_g, cores) = host_size();
    r.line(format!("host: {cores} cores, {mem_g} GiB"));
    if cores != 2 {
        r.problem(format!("expected 2 cores on r8g.large, found {cores}"));
    }
    match root_fs() {
        Some((total, free)) => {
            r.line(format!(
                "root filesystem: {} total, {} free",
                gib(total),
                gib(free)
            ));
            if !(NEW_ROOT_MIN_FS_BYTES..=NEW_ROOT_MAX_FS_BYTES).contains(&total) {
                r.problem(format!(
                    "root filesystem is {}, not the 150 GB root",
                    gib(total)
                ));
            }
        }
        None => r.problem("root filesystem could not be read"),
    }
    let (_, high) = systemctl(&["show", "-p", "MemoryHigh", "tickvault"]);
    r.line(format!("tickvault.service {high}"));
    let env = std::fs::read_to_string(crate::host_tuning::COMPOSE_ENV_PATH).unwrap_or_default();
    for key in ["QDB_MEM_LIMIT", "TV_QDB_CPUSET", "TV_QDB_CPUS"] {
        let v = env
            .lines()
            .filter_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .next_back()
            .unwrap_or("(compose default)");
        r.line(format!("compose env {key}={v}"));
    }
    let q = match questdb() {
        Ok(q) => q,
        Err(e) => {
            r.problem(e);
            return r;
        }
    };
    let names = q.table_names().await.unwrap_or_default();
    r.line(format!("QuestDB tables: {}", names.len()));
    for t in QUOTE_34_REMOVED_TABLES {
        if names.iter().any(|n| n == t) {
            r.line(format!(
                "{t} exists: the deployed build predates ITEM 62 and still creates it (Quote 34); the rebuild did not restore it"
            ));
        }
    }
    if let Some(key) = manifest_key {
        let cold = RebuildCold::connect(COLD_BUCKET).await;
        std::fs::create_dir_all("data/tmp").ok(); // the write that follows reports a failure
        match cold
            .get_small(key, Path::new("data/tmp"))
            .await
            .and_then(|b| serde_json::from_slice::<CopyManifest>(&b).map_err(|e| e.to_string()))
        {
            Ok(m) => {
                for keep in &m.keep_tables {
                    match q.count(&keep.data.table).await {
                        Ok(n) if n >= keep.data.rows => {}
                        Ok(n) => r.problem(format!(
                            "{}: {n} rows, fewer than the {} restored",
                            keep.data.table, keep.data.rows
                        )),
                        Err(e) => r.problem(format!("{}: {e}", keep.data.table)),
                    }
                }
                r.line(format!("keep tables checked: {}", m.keep_tables.len()));
            }
            Err(e) => r.problem(format!("manifest: {e}")),
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    const SHA: &str = "f92a6472f0000000000000000000000000000000";
    const KEY: &str = "rebuild-2026-10-22/manifests/copy-20261022T133500Z.json";

    #[test]
    fn test_is_invocation_matches_only_the_subcommand_word() {
        assert!(is_invocation(&args(&["tickvault", "rebuild-box", "copy"])));
        assert!(!is_invocation(&args(&["tickvault", "ensure-questdb"])));
        assert!(!is_invocation(&args(&["tickvault"])));
    }

    #[test]
    fn test_parse_args_validates_every_word() {
        let ok = parse_args(&args(&["t", "rebuild-box", "copy", "dry", SHA, "-"])).expect("ok");
        assert_eq!(ok.phase, Phase::Copy);
        assert_eq!(ok.manifest, None);
        let ok = parse_args(&args(&[
            "t",
            "rebuild-box",
            "prepare-new",
            "apply",
            SHA,
            KEY,
        ]))
        .expect("ok");
        assert_eq!(ok.manifest.as_deref(), Some(KEY));
        for bad in [
            vec!["t", "rebuild-box", "swap", "apply", SHA, "-"],
            vec!["t", "rebuild-box", "copy", "maybe", SHA, "-"],
            vec!["t", "rebuild-box", "copy", "dry", "abc", "-"],
            vec!["t", "rebuild-box", "postcheck", "dry", SHA, "x; reboot"],
            vec!["t", "rebuild-box", "prepare-new", "apply", SHA, "-"],
            vec!["t", "rebuild-box", "prepare-new", "dry", SHA, KEY],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_plan_table_routes_each_class() {
        let keep = ["instrument_lifecycle", "order_audit", "top_volume_rank"];
        assert_eq!(
            plan_table("instrument_lifecycle", "NONE", "ts", &keep),
            TablePlan::Keep
        );
        // On the keep list but removed by Quote 34: copied, never restored.
        assert_eq!(
            plan_table("top_volume_rank", "HOUR", "ts", &keep),
            TablePlan::Partitions
        );
        assert_eq!(
            plan_table("ticks", "HOUR", "ts", &keep),
            TablePlan::Partitions
        );
        assert_eq!(
            plan_table("candles_3s", "DAY", "ts", &keep),
            TablePlan::Partitions
        );
        assert_eq!(plan_table("x", "NONE", "", &keep), TablePlan::Whole);
        assert_eq!(plan_table("y", "DAY", "time", &keep), TablePlan::Whole);
        assert_eq!(
            plan_table("telemetry", "NONE", "", &keep),
            TablePlan::Internal
        );
        assert_eq!(
            plan_table("sys.telemetry_wal", "DAY", "ts", &keep),
            TablePlan::Internal
        );
        assert_eq!(
            plan_table("Bad'Name", "DAY", "ts", &keep),
            TablePlan::Unsafe
        );
    }

    #[test]
    fn test_is_wal_segment_name_matches_sealed_segments_only() {
        assert!(is_wal_segment_name("ws-frames-1761100000000000000.wal"));
        assert!(!is_wal_segment_name("ws-frames-.wal"));
        assert!(!is_wal_segment_name("ws-frames-12.wal.tmp"));
        assert!(!is_wal_segment_name("applied.tvaw"));
    }

    #[test]
    fn test_cloud_init_summary_reads_the_errors_list() {
        assert_eq!(
            cloud_init_summary(br#"{"v1":{"datasource":"Ec2","errors":[]}}"#),
            "done"
        );
        assert_eq!(
            cloud_init_summary(br#"{"v1":{"errors":["a failed","b failed"]}}"#),
            "done with 2 error(s): a failed"
        );
        assert_eq!(
            cloud_init_summary(br#"{"v2":{}}"#),
            "done, result.json has no v1.errors"
        );
        assert_eq!(
            cloud_init_summary(b"not json"),
            "done, result.json did not parse"
        );
    }

    #[test]
    fn test_unapplied_wal_tables_names_lagging_and_suspended_tables() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"columns":[{"name":"name"},{"name":"suspended"},{"name":"writerTxn"},{"name":"sequencerTxn"}],
               "dataset":[["a",false,5,5],["b",false,4,5],["c",true,5,5]]}"#,
        )
        .expect("json");
        assert_eq!(
            unapplied_wal_tables(&v),
            Some(vec!["b".to_string(), "c".to_string()])
        );
        let bad: serde_json::Value =
            serde_json::from_str(r#"{"columns":[],"dataset":[]}"#).expect("json");
        assert_eq!(unapplied_wal_tables(&bad), None);
    }

    #[test]
    fn test_parse_tables_reads_by_column_name_and_sorts() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"columns":[{"name":"designatedTimestamp"},{"name":"table_name"},{"name":"partitionBy"}],
               "dataset":[["ts","ticks","HOUR"],[null,"x","NONE"]]}"#,
        )
        .expect("json");
        assert_eq!(
            parse_tables(&v),
            Some(vec![
                ("ticks".to_string(), "HOUR".to_string(), "ts".to_string()),
                ("x".to_string(), "NONE".to_string(), String::new()),
            ])
        );
    }

    #[test]
    fn test_gib_renders_one_decimal() {
        assert_eq!(gib(1_073_741_824), "1.0 GiB");
        assert_eq!(gib(0), "0.0 GiB");
    }
}
