//! `tickvault wipe-box <dry|apply> <sha>`: the box-side half of the one-off
//! market-data wipe (plan item 67, operator Quotes 35 and 42; see `wipe_2026_10_31`).
//!
//! Sent over SSM by the runner as the one budgeted shell element in
//! `wipe_ssm`: a copy of the deployed binary for the workflow's commit is
//! fetched to `/var/tmp` and run with this subcommand, as root. It never runs
//! from the app's own boot or from the systemd units.
//!
//! | Step | Dry | Apply |
//! |---|---|---|
//! | app | reports `systemctl is-active tickvault` | stops it; refuses every delete unless it then reads `inactive` / `failed` |
//! | files | lists `data/ws_wal`, `data/spill`, `data/dlq`, counts what `box_file_verdict` deletes and keeps | deletes exactly those files, lists again, fails if one is left |
//! | QuestDB | lists every market-data table's partitions and the action `table_action` takes | waits for every market-data table's WAL to apply, then `TRUNCATE TABLE` or `DROP PARTITION LIST` of the old partitions only; lists again until none is left |
//! | detached | lists `<table>/<partition>.detached` directories `detached_dir_verdict` deletes | deletes them, lists again |
//!
//! Nothing on the keep-list is ever touched: the classifiers refuse every
//! SEBI and audit table, `data/spill/audit`, any `sebi-preserve` path and the
//! WAL's state files. The app is left stopped (still enabled); the runner
//! stops the instance afterwards. Every check that fails is a problem in the
//! one report line it prints (`WIPE-REPORT {json}`).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::wipe_2026_10_31::{
    BoxReport, CUTOFF_IST_DATE, Mode, TableAction, Verdict, app_state_permits_delete,
    box_file_verdict, cutoff_unix_secs, detached_dir_verdict, is_commit_sha, is_keep_table,
    market_data_tables, partition_verdict, table_action,
};

/// The CLI word.
pub const WIPE_BOX_SUBCOMMAND: &str = "wipe-box";

/// The app's working directory on the box.
const BOX_ROOT: &str = "/opt/tickvault";
/// The three trees the file step walks, relative to [`BOX_ROOT`].
const FILE_ROOTS: [&str; 3] = ["data/ws_wal", "data/spill", "data/dlq"];
/// QuestDB's table directories on the host: the named Docker volume
/// `tv-questdb-data` (mounted at `/var/lib/questdb` in the container) under
/// Docker's default data root. Assumed; a missing directory is reported,
/// never guessed at.
const QUESTDB_DB_ROOT: &str = "/var/lib/docker/volumes/tv-questdb-data/_data/db";
/// QuestDB's HTTP port, published on the host's loopback by the compose file.
const QUESTDB_HTTP_PORT: u16 = 9000;
/// How long to wait for every market-data table's WAL to apply.
const WAL_APPLY_WAIT_SECS: u64 = 1800;
/// How long to wait for the drops to become visible.
const DROP_VISIBLE_WAIT_SECS: u64 = 900;
/// Poll interval for both waits.
const POLL_SECS: u64 = 5;
/// HTTP timeout for one QuestDB statement.
const QUESTDB_TIMEOUT_SECS: u64 = 300;
/// Partition names per `DROP PARTITION LIST` statement.
const DROP_LIST_CHUNK: usize = 50;
/// Where the box keeps its full local report.
const LOCAL_REPORT_DIR: &str = "/var/tmp";

/// True when the process was started as `tickvault wipe-box ...`.
#[must_use]
pub fn is_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(WIPE_BOX_SUBCOMMAND)
}

/// One line to stdout (the SSM output). A failed write has nowhere to go.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "wipe-box: {line}").unwrap_or_default();
}

/// Parses `wipe-box <mode> <sha>`.
///
/// # Errors
/// The first argument that fails validation.
pub fn parse_wipe_box_args(args: &[String]) -> Result<(Mode, String), String> {
    let get = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    let mode = Mode::parse(get(2)).ok_or_else(|| format!("mode {:?}", get(2)))?;
    let sha = get(3);
    if !is_commit_sha(sha) {
        return Err(format!("commit {sha:?}"));
    }
    if args.len() > 4 {
        return Err(format!("{} unexpected argument(s)", args.len() - 4));
    }
    Ok((mode, sha.to_string()))
}

/// Runs the subcommand; the process exit code (0 only when the report is ok).
// TEST-EXEMPT: process entry point on the box; the decisions are the tested pure functions it calls
pub async fn run(args: &[String]) -> i32 {
    let report = match parse_wipe_box_args(args) {
        Err(why) => {
            let mut r = BoxReport::new(Mode::Dry);
            r.problem(format!("bad arguments: {why}"));
            r
        }
        Ok((mode, sha)) => {
            emit(&format!(
                "{} starting (binary of commit {sha})",
                mode.as_str()
            ));
            let mut r = BoxReport::new(mode);
            if let Err(err) = std::env::set_current_dir(BOX_ROOT) {
                r.problem(format!("cannot enter {BOX_ROOT}: {err}"));
            } else {
                wipe(mode, &mut r).await;
            }
            r
        }
    };
    let (report, line) = report.finish();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let local =
        PathBuf::from(LOCAL_REPORT_DIR).join(format!("tv-wipe-box-{}-{stamp}.json", report.mode));
    if let Ok(full) = serde_json::to_vec_pretty(&report) {
        std::fs::write(&local, full).ok(); // the report line below is the record that matters
    }
    {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").unwrap_or_default();
    }
    i32::from(!report.ok)
}

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

async fn wipe(mode: Mode, r: &mut BoxReport) {
    if cutoff_unix_secs().is_none() {
        r.problem("internal: the cutoff does not parse");
        return;
    }
    // 1. The app.
    let (_, state) = systemctl(&["is-active", "tickvault"]);
    r.line(format!("app state before: {state}"));
    if mode == Mode::Apply {
        let (stopped, out) = systemctl(&["stop", "tickvault"]);
        if !stopped {
            r.problem(format!("systemctl stop tickvault failed: {out}"));
        }
        let (_, after) = systemctl(&["is-active", "tickvault"]);
        r.line(format!("app state after stop: {after}"));
        if !app_state_permits_delete(&after) {
            r.problem(format!("the app is not stopped ({after}); nothing deleted"));
            return;
        }
    } else if !app_state_permits_delete(&state) {
        r.line("dry: the app is running; apply stops it before any delete");
    }

    // 2. Files.
    let base = PathBuf::from(".");
    let plan = plan_files(&base);
    r.line(plan.summary("files"));
    for other in plan.unreadable.iter().take(10) {
        r.line(format!(
            "file kept, not readable as a regular file: {other}"
        ));
    }
    if mode == Mode::Apply {
        for err in delete_files(&plan) {
            r.problem(err);
        }
        let after = plan_files(&base);
        r.line(after.summary("files after"));
        if !after.delete.is_empty() {
            r.problem(format!(
                "{} old market-data file(s) still on disk after the delete",
                after.delete.len()
            ));
        }
    }

    // 3. QuestDB partitions.
    match QuestDb::new() {
        Ok(q) => questdb_step(&q, mode, r).await,
        Err(e) => r.problem(format!("QuestDB client: {e}")),
    }

    // 4. Detached partition directories.
    let root = Path::new(QUESTDB_DB_ROOT);
    if root.is_dir() {
        let detached = plan_detached(root);
        r.line(format!(
            "detached partition dirs to delete: {} ({})",
            detached.len(),
            gib(detached.iter().map(|d| d.1).sum())
        ));
        if mode == Mode::Apply {
            for (dir, _) in &detached {
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    r.problem(format!("removing {}: {e}", dir.display()));
                }
            }
            let left = plan_detached(root).len();
            if left > 0 {
                r.problem(format!(
                    "{left} old detached partition dir(s) still on disk"
                ));
            }
        }
    } else {
        r.line(format!(
            "{QUESTDB_DB_ROOT} is not a directory: detached partitions not checked"
        ));
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / 1_073_741_824.0)
}

/// What the file step deletes and keeps.
#[derive(Debug, Default)]
struct FilePlan {
    delete: Vec<(PathBuf, u64)>,
    kept_not_old: usize,
    kept_protected: usize,
    kept_unparsed: usize,
    unreadable: Vec<String>,
}

impl FilePlan {
    fn summary(&self, what: &str) -> String {
        format!(
            "{what}: delete {} ({}); kept {} dated {CUTOFF_IST_DATE} or later, {} on the keep-list, {} other",
            self.delete.len(),
            gib(self.delete.iter().map(|d| d.1).sum()),
            self.kept_not_old,
            self.kept_protected,
            self.kept_unparsed
        )
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(t.as_secs()).ok()
}

/// Walks [`FILE_ROOTS`] under `base` (symlinks never followed) and sorts
/// every regular file by [`box_file_verdict`]. O(files).
fn plan_files(base: &Path) -> FilePlan {
    let mut plan = FilePlan::default();
    for root in FILE_ROOTS {
        let mut stack = vec![PathBuf::from(root)];
        while let Some(rel_dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(base.join(&rel_dir)) else {
                continue; // an absent tree has nothing to delete
            };
            for entry in entries.flatten() {
                let rel = rel_dir.join(entry.file_name());
                let rel_str = rel.to_string_lossy().to_string();
                let Ok(meta) = std::fs::symlink_metadata(base.join(&rel)) else {
                    plan.unreadable.push(rel_str);
                    continue;
                };
                if meta.is_dir() {
                    stack.push(rel);
                    continue;
                }
                if !meta.is_file() {
                    plan.unreadable.push(rel_str);
                    continue;
                }
                let Some(t) = mtime_secs(&meta) else {
                    plan.unreadable.push(rel_str);
                    continue;
                };
                match box_file_verdict(&rel_str, t) {
                    Verdict::Delete => plan.delete.push((base.join(&rel), meta.len())),
                    Verdict::KeepNotOld => plan.kept_not_old += 1,
                    Verdict::KeepProtected => plan.kept_protected += 1,
                    Verdict::KeepUnparsed => plan.kept_unparsed += 1,
                }
            }
        }
    }
    plan.delete.sort();
    plan.unreadable.sort();
    plan
}

/// Deletes exactly the planned files; one error line per failure.
fn delete_files(plan: &FilePlan) -> Vec<String> {
    plan.delete
        .iter()
        .filter_map(|(p, _)| {
            std::fs::remove_file(p)
                .err()
                .map(|e| format!("removing {}: {e}", p.display()))
        })
        .collect()
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Every `<table dir>/<partition>.detached` directory under `root` that
/// [`detached_dir_verdict`] deletes, with its size.
fn plan_detached(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(tables) = std::fs::read_dir(root) else {
        return out;
    };
    for t in tables.flatten() {
        let table_dir = t.file_name().to_string_lossy().to_string();
        if !t.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(t.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type().is_ok_and(|ft| ft.is_dir())
                && detached_dir_verdict(&table_dir, &name) == Verdict::Delete
            {
                out.push((e.path(), dir_bytes(&e.path())));
            }
        }
    }
    out.sort();
    out
}

/// A minimal QuestDB `/exec` client on the host's loopback.
struct QuestDb {
    client: reqwest::Client,
    url: String,
}

impl QuestDb {
    fn new() -> Result<Self, String> {
        // Host-side tool: QuestDB publishes 9000 on the host's loopback (the
        // compose default bind); there is no Docker DNS name outside a container.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(QUESTDB_TIMEOUT_SECS))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            url: format!(
                "http://{}:{QUESTDB_HTTP_PORT}/exec",
                std::net::Ipv4Addr::LOCALHOST
            ),
        })
    }

    async fn exec(&self, sql: &str) -> Result<serde_json::Value, String> {
        let resp = self
            .client
            .get(&self.url)
            .query(&[("query", sql)])
            .send()
            .await
            .map_err(|e| format!("QuestDB request failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("QuestDB body read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "QuestDB returned {status}: {}",
                tickvault_common::sanitize::capture_rest_error_body(&body)
            ));
        }
        serde_json::from_str(&body).map_err(|e| format!("QuestDB answer is not JSON: {e}"))
    }
}

/// Rows of a QuestDB `/exec` answer as maps from column name to cell.
fn rows(
    json: &serde_json::Value,
) -> Option<Vec<std::collections::HashMap<String, serde_json::Value>>> {
    let names: Vec<String> = json
        .get("columns")?
        .as_array()?
        .iter()
        .map(|c| c.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect::<Option<Vec<String>>>()?;
    let mut out = Vec::new();
    for row in json.get("dataset")?.as_array()? {
        let cells = row.as_array()?;
        out.push(names.iter().cloned().zip(cells.iter().cloned()).collect());
    }
    Some(out)
}

/// The non-detached partition names of one `table_partitions()` answer.
/// `None` when the answer does not have the expected columns.
fn attached_partitions(json: &serde_json::Value) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for row in rows(json)? {
        let name = row.get("name")?.as_str()?.to_string();
        let detached = row.get("detached")?.as_bool()?;
        if !detached {
            out.push(name);
        }
    }
    out.sort();
    Some(out)
}

/// Market-data tables whose WAL is not fully applied (suspended, or
/// `writerTxn != sequencerTxn`). `None` when the answer does not parse.
fn unapplied_market_tables(json: &serde_json::Value) -> Option<Vec<String>> {
    let md = market_data_tables();
    let mut out = Vec::new();
    for row in rows(json)? {
        let name = row.get("name")?.as_str()?.to_string();
        let suspended = row.get("suspended")?.as_bool()?;
        let (w, s) = (
            row.get("writerTxn")?.as_i64()?,
            row.get("sequencerTxn")?.as_i64()?,
        );
        if md.contains(&name.as_str()) && (suspended || w != s) {
            out.push(name);
        }
    }
    Some(out)
}

/// The statements one [`TableAction`] runs, every name already validated.
fn action_sql(table: &str, action: &TableAction) -> Vec<String> {
    match action {
        TableAction::Truncate(_) => vec![format!("TRUNCATE TABLE {table}")],
        TableAction::DropList(names) => names
            .chunks(DROP_LIST_CHUNK)
            .map(|chunk| {
                let list = chunk
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("ALTER TABLE {table} DROP PARTITION LIST {list}")
            })
            .collect(),
        TableAction::Untouched(_) | TableAction::Nothing => Vec::new(),
    }
}

async fn partitions_of(q: &QuestDb, table: &str) -> Result<Vec<String>, String> {
    let v = q
        .exec(&format!(
            "SELECT name, detached FROM table_partitions('{table}')"
        ))
        .await?;
    attached_partitions(&v).ok_or_else(|| format!("table_partitions('{table}') did not parse"))
}

async fn questdb_step(q: &QuestDb, mode: Mode, r: &mut BoxReport) {
    let tables = match q.exec("SELECT table_name FROM tables()").await {
        Ok(v) => match rows(&v) {
            Some(rs) => rs
                .iter()
                .filter_map(|row| {
                    row.get("table_name")
                        .and_then(|c| c.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>(),
            None => {
                r.problem("tables() did not parse");
                return;
            }
        },
        Err(e) => {
            r.problem(format!("tables(): {e}"));
            return;
        }
    };
    let md = market_data_tables();
    let kept = tables.iter().filter(|t| is_keep_table(t)).count();
    r.line(format!(
        "QuestDB: {} tables, {kept} on the SEBI/audit keep-list (untouched)",
        tables.len()
    ));
    let targets: Vec<&String> = tables.iter().filter(|t| md.contains(&t.as_str())).collect();

    if mode == Mode::Apply {
        let deadline = Instant::now() + Duration::from_secs(WAL_APPLY_WAIT_SECS);
        loop {
            match q
                .exec("SELECT name, suspended, writerTxn, sequencerTxn FROM wal_tables()")
                .await
                .map(|v| unapplied_market_tables(&v))
            {
                Ok(Some(left)) if left.is_empty() => break,
                Ok(Some(left)) if Instant::now() >= deadline => {
                    r.problem(format!(
                        "WAL not applied after {WAL_APPLY_WAIT_SECS} s for: {}; no partition dropped",
                        left.join(", ")
                    ));
                    return;
                }
                Ok(None) => {
                    r.problem("wal_tables() did not parse; no partition dropped");
                    return;
                }
                Err(e) => {
                    r.problem(format!("wal_tables(): {e}; no partition dropped"));
                    return;
                }
                Ok(Some(_)) => tokio::time::sleep(Duration::from_secs(POLL_SECS)).await,
            }
        }
    }

    let mut pending: Vec<String> = Vec::new();
    for table in targets {
        let parts = match partitions_of(q, table).await {
            Ok(p) => p,
            Err(e) => {
                r.problem(e);
                continue;
            }
        };
        let action = table_action(table, &parts);
        let summary = match &action {
            TableAction::Truncate(n) => {
                format!("truncate ({n} partitions, all before {CUTOFF_IST_DATE})")
            }
            TableAction::DropList(v) => format!(
                "drop {} of {} partitions (the rest are {CUTOFF_IST_DATE} or later, or unrecognised)",
                v.len(),
                parts.len()
            ),
            TableAction::Nothing => format!("nothing to drop ({} partitions)", parts.len()),
            TableAction::Untouched(v) => format!("untouched ({v:?})"),
        };
        r.line(format!("{table}: {summary}"));
        if mode == Mode::Apply {
            let sqls = action_sql(table, &action);
            if !sqls.is_empty() {
                pending.push(table.clone());
            }
            for sql in sqls {
                if let Err(e) = q.exec(&sql).await {
                    r.problem(format!("{table}: {e}"));
                }
            }
        }
    }
    if mode == Mode::Dry {
        return;
    }
    // Verify: no attached partition dated before the cutoff remains.
    let deadline = Instant::now() + Duration::from_secs(DROP_VISIBLE_WAIT_SECS);
    while !pending.is_empty() {
        let mut still = Vec::new();
        for table in &pending {
            match partitions_of(q, table).await {
                Ok(p)
                    if p.iter()
                        .any(|n| partition_verdict(table, n) == Verdict::Delete) =>
                {
                    still.push(table.clone());
                }
                Ok(_) => r.line(format!(
                    "{table}: verified, no partition before {CUTOFF_IST_DATE}"
                )),
                Err(e) => {
                    r.problem(e);
                }
            }
        }
        if still.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            r.problem(format!(
                "old partitions still present after {DROP_VISIBLE_WAIT_SECS} s: {}",
                still.join(", ")
            ));
            break;
        }
        pending = still;
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    const SHA: &str = "d1b4835470000000000000000000000000000000";

    #[test]
    fn test_is_invocation_matches_only_the_subcommand_word() {
        assert!(is_invocation(&args(&["tickvault", "wipe-box", "dry"])));
        assert!(!is_invocation(&args(&["tickvault", "rebuild-box"])));
        assert!(!is_invocation(&args(&["tickvault"])));
    }

    #[test]
    fn test_parse_wipe_box_args_validates_every_word() {
        assert_eq!(
            parse_wipe_box_args(&args(&["t", "wipe-box", "dry", SHA])),
            Ok((Mode::Dry, SHA.to_string()))
        );
        assert_eq!(
            parse_wipe_box_args(&args(&["t", "wipe-box", "apply", SHA])),
            Ok((Mode::Apply, SHA.to_string()))
        );
        assert!(parse_wipe_box_args(&args(&["t", "wipe-box", "now", SHA])).is_err());
        assert!(parse_wipe_box_args(&args(&["t", "wipe-box", "dry", "abc"])).is_err());
        assert!(parse_wipe_box_args(&args(&["t", "wipe-box", "dry", SHA, "x"])).is_err());
        assert!(parse_wipe_box_args(&args(&["t", "wipe-box"])).is_err());
    }

    fn touch(base: &Path, rel: &str, mtime_secs: u64) {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let f = std::fs::File::create(&p).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs))
            .unwrap();
    }

    fn temp_tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-wipe-box-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const OLD: u64 = 1_793_000_000; // 2026-10-26, before the cutoff
    // 2026-10-28 00:00 IST exactly: the cutoff instant itself is kept.
    const NEW: u64 = 1_793_125_800;

    fn seed(base: &Path) {
        for rel in [
            "data/ws_wal/ws-frames-1.wal",
            "data/ws_wal/replaying/ws-frames-2.wal",
            "data/spill/ticks/a.ilp",
            "data/spill/seals_v6-x.bin",
            "data/dlq/seals.ndjson",
        ] {
            touch(base, rel, OLD);
        }
        for rel in [
            "data/ws_wal/applied.tvaw",
            "data/ws_wal/.wal-owner.lock",
            "data/spill/audit/order_audit/a.ilp",
            "data/instruments/cache.rkyv",
        ] {
            touch(base, rel, OLD);
        }
        touch(base, "data/spill/ticks/new.ilp", NEW);
        touch(base, "data/ws_wal/ws-frames-9.wal", NEW);
    }

    #[test]
    fn test_plan_files_dry_run_deletes_nothing() {
        let base = temp_tree();
        seed(&base);
        let plan = plan_files(&base);
        assert_eq!(plan.delete.len(), 5, "{plan:?}");
        assert_eq!(plan.kept_not_old, 2);
        assert_eq!(plan.kept_protected, 3);
        // Planning twice (the dry run) changes nothing on disk.
        let again = plan_files(&base);
        assert_eq!(again.delete, plan.delete);
        for (p, _) in &plan.delete {
            assert!(p.exists(), "{}", p.display());
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_delete_files_removes_only_planned_files_and_keeps_the_keep_list() {
        let base = temp_tree();
        seed(&base);
        let plan = plan_files(&base);
        assert!(delete_files(&plan).is_empty());
        let after = plan_files(&base);
        assert!(after.delete.is_empty());
        for rel in [
            "data/ws_wal/applied.tvaw",
            "data/ws_wal/.wal-owner.lock",
            "data/spill/audit/order_audit/a.ilp",
            "data/instruments/cache.rkyv",
            "data/spill/ticks/new.ilp",
            "data/ws_wal/ws-frames-9.wal",
        ] {
            assert!(base.join(rel).exists(), "{rel} must be kept");
        }
        assert!(!base.join("data/spill/ticks/a.ilp").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn test_plan_detached_only_old_market_data_dirs() {
        let base = temp_tree();
        for d in [
            "ticks~3/2026-10-01T09.detached",
            "ticks~3/2026-10-28T09.detached",
            "ticks~3/2026-10-01T09",
            "order_audit~4/2026-10-01.detached",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        touch(&base, "ticks~3/2026-10-01T09.detached/ts.d", OLD);
        let found = plan_detached(&base);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].0.ends_with("ticks~3/2026-10-01T09.detached"));
        std::fs::remove_dir_all(&base).ok();
    }

    fn answer(columns: &[&str], data: serde_json::Value) -> serde_json::Value {
        let cols: Vec<serde_json::Value> = columns
            .iter()
            .map(|c| serde_json::json!({ "name": c }))
            .collect();
        serde_json::json!({ "columns": cols, "dataset": data })
    }

    #[test]
    fn test_attached_partitions_skips_detached_and_needs_the_columns() {
        let v = answer(
            &["name", "detached"],
            serde_json::json!([
                ["2026-10-02T09", false],
                ["2026-10-01T09", true],
                ["2026-10-01T10", false]
            ]),
        );
        assert_eq!(
            attached_partitions(&v),
            Some(vec![
                "2026-10-01T10".to_string(),
                "2026-10-02T09".to_string()
            ])
        );
        assert_eq!(
            attached_partitions(&answer(&["name"], serde_json::json!([["x"]]))),
            None
        );
    }

    #[test]
    fn test_unapplied_market_tables_names_only_lagging_market_tables() {
        let v = answer(
            &["name", "suspended", "writerTxn", "sequencerTxn"],
            serde_json::json!([
                ["ticks", false, 5, 7],
                ["candles_1m", true, 3, 3],
                ["market_depth", false, 9, 9],
                ["order_audit", false, 1, 2]
            ]),
        );
        assert_eq!(
            unapplied_market_tables(&v),
            Some(vec!["ticks".to_string(), "candles_1m".to_string()])
        );
        assert_eq!(unapplied_market_tables(&serde_json::json!({})), None);
    }

    #[test]
    fn test_action_sql_truncates_or_drops_in_chunks() {
        assert_eq!(
            action_sql("ticks", &TableAction::Truncate(3)),
            vec!["TRUNCATE TABLE ticks".to_string()]
        );
        let names: Vec<String> = (0..120)
            .map(|i| format!("2026-10-{:02}T09", 1 + i % 28))
            .collect();
        let sql = action_sql("ticks", &TableAction::DropList(names));
        assert_eq!(sql.len(), 3);
        assert!(sql[0].starts_with("ALTER TABLE ticks DROP PARTITION LIST '2026-10-01T09', "));
        assert!(action_sql("ticks", &TableAction::Nothing).is_empty());
        assert!(
            action_sql(
                "order_audit",
                &TableAction::Untouched(Verdict::KeepProtected)
            )
            .is_empty()
        );
    }
}
