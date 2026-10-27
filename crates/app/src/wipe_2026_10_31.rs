//! The one-off full market-data wipe (plan item 67; operator Quote 35,
//! brought forward by Quote 42, both in
//! `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`).
//!
//! Quote 35 named the data and stated the loss: every market-data partition
//! and file on the box and in `s3://tv-prod-cold`, every version, plus the
//! snapshot of the old 600 GB root from the 2026-10-22 rebuild. Quote 42
//! (2026-10-27) moved it to the night of Tuesday 2026-10-27: the apply window
//! opens 2026-10-27 18:00 IST, the cutoff is **2026-10-28** (nothing
//! Wednesday's fresh box writes can be touched), the S3 scope adds the
//! market-data tables the rebuild copied under `rebuild-2026-10-22/tables/`
//! and the three quarantine prefixes, and the old-root snapshot is kept until
//! 2026-10-28 16:30 IST. Quote 29's keep-list is kept: every SEBI and audit
//! table (the rebuild's keep-list, [`is_keep_table`]), `sebi-preserve/`,
//! `deploys/`, `logs/`, versioning, lifecycle, and anything dated 2026-10-28
//! or later. The tool's name, workflow, environment, role and confirm word
//! keep their 2026-10-31 names (renaming them the same day is not possible).
//!
//! This module is the pure half shared by the two programs that run it:
//!
//! - `tv-wipe-2026-10-31` (`src/bin/wipe_2026_10_31.rs`), run by
//!   `.github/workflows/wipe-2026-10-31.yml`: EC2, SSM, the S3 version
//!   delete and the snapshot delete.
//! - `tickvault wipe-box <mode> <sha>` (`wipe_box.rs`), run ON the box over
//!   SSM by a copy of the deployed binary: stops the app, deletes the old
//!   files, drops the old partitions.
//!
//! Every decision about what is deleted is a function here, tested against
//! the keep-list and the date fence; nothing comes from config. A key, file
//! or partition that does not parse into a known shape is KEPT and reported,
//! never guessed at.
//!
//! Cold path, one-off. Removed (with the workflow, the box subcommand, the
//! role and the budget rows) by the first PR after the run.
//!
//! Complexity: every classifier is O(name length); a plan is O(entries).

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// The only bucket this tool may touch.
pub const COLD_BUCKET: &str = "tv-prod-cold";
/// Data dated strictly before this IST date is in scope (Quote 42: "Data
/// dated before 2026-10-28", so nothing Wednesday's fresh box writes can be
/// touched). Was 2026-11-01 under Quote 35.
pub const CUTOFF_IST_DATE: &str = "2026-10-28";
/// Every run is refused after this IST date (Quote 35: refused after
/// 2026-11-01 IST; Quote 42 keeps the end). The IAM role expires at the same
/// instant.
pub const LAST_RUN_IST_DATE: &str = "2026-11-01";
/// An apply may start no earlier than this IST instant (Quote 42: "Apply
/// allowed from 2026-10-27 18:00 IST"; was 2026-10-31 18:00 under Quote 35).
pub const APPLY_FROM_IST: &str = "2026-10-27 18:00";
/// The old-root snapshot may be deleted no earlier than this IST instant
/// (Quote 42: "no earlier than 2026-10-28 16:30 IST, after Wednesday's first
/// session on the new box ends, so a failed rebuild can still be undone
/// overnight"). Checked by [`snapshot_delete_refusal`].
pub const SNAPSHOT_DELETE_FROM_IST: &str = "2026-10-28 16:30";
/// An apply may start no later than this IST instant (before Sunday
/// morning; nowhere near 09:00–15:45 IST).
pub const APPLY_UNTIL_IST: &str = "2026-11-01 08:00";
/// `--confirm` value an `apply` must carry.
pub const APPLY_CONFIRM: &str = "WIPE-BEFORE-2026-11-01";
/// S3 `DeleteObjects` accepts at most 1,000 keys per request.
pub const DELETE_BATCH_MAX: usize = 1000;
/// Tag the 2026-10-22 rebuild put on the old-root snapshot
/// (`tv-rebuild-2026-10-22`, phase `snapshot`).
pub const SNAPSHOT_TAG_KEY: &str = "tv-rebuild";
/// Value of [`SNAPSHOT_TAG_KEY`].
pub const SNAPSHOT_TAG_VALUE: &str = "2026-10-22";
/// `Name` tag the rebuild put on the old-root snapshot.
pub const SNAPSHOT_NAME: &str = "tv-prod-root-before-rebuild-2026-10-22";
/// Tag the rebuild put on the old 600 GB root VOLUME (kept detached). Quote 35
/// does not name the volume, so the tool only reports it.
pub const OLD_ROOT_TAG_KEY: &str = "tv-rebuild-old-root";
/// Prefix of the one JSON line `wipe-box` prints for the runner.
pub const BOX_REPORT_PREFIX: &str = "WIPE-REPORT ";
/// Most problem lines and fact lines a box report carries (SSM returns at
/// most 24,000 characters of output).
const REPORT_MAX_PROBLEMS: usize = 40;
const REPORT_MAX_LINES: usize = 80;
const REPORT_LINE_MAX_CHARS: usize = 160;

const PARTITIONS_ROOT: &str = "questdb-partitions/";
/// Dated S3 roots: `<root><IST date>/<file>`. The first three are Quote 35's;
/// the three quarantine roots are Quote 42's ("market data the owner named
/// in 'entire wipe off inclduign s3'"), written by `raw_frame_upload` with
/// the same `<prefix>/<IST date>/<name>.gz` key shape.
const DATED_ROOTS: [&str; 6] = [
    "raw-frames/",
    "seal-spill/",
    "top-volume-spill/",
    "tick-quarantine/",
    "depth-quarantine/",
    "top-volume-quarantine/",
];
/// Where the 2026-10-22 rebuild put the tables it copied WHOLE:
/// `rebuild-2026-10-22/tables/<table>/<sha256 hex>.csv.gz`
/// (`partition_archive::rebuild_whole_table_key`). Quote 42 adds the
/// market-data tables here; keep-list tables here are kept. The key has no
/// date, so its object's last-modified time dates it.
pub const REBUILD_TABLES_ROOT: &str = "rebuild-2026-10-22/tables/";
/// The rebuild's keep-list copies (`rebuild-2026-10-22/keep/`): never touched.
const REBUILD_KEEP_ROOT: &str = "rebuild-2026-10-22/keep/";
/// S3 roots that are never touched, whatever follows.
const PROTECTED_ROOTS: [&str; 3] = ["deploys/", "sebi-preserve/", "logs/"];
/// The only year the IAM grant covers (`2026-0?-??*`, `2026-10-??*`).
const GRANTED_YEAR: &str = "2026";
const IST_OFFSET_SECS: i32 = 5 * 3600 + 30 * 60;
/// No apply in 09:00–15:45 IST, whatever the date fence says.
const MARKET_FROM_HHMM: (u32, u32) = (9, 0);
const MARKET_UNTIL_HHMM: (u32, u32) = (15, 45);

/// What one run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Report only; deletes nothing.
    Dry,
    /// Delete.
    Apply,
}

impl Mode {
    /// Parses `dry` / `apply`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dry" => Some(Self::Dry),
            "apply" => Some(Self::Apply),
            _ => None,
        }
    }

    /// Wire word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dry => "dry",
            Self::Apply => "apply",
        }
    }
}

/// One step of the wipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Read-only facts (the instance, the account); never deletes.
    Preflight,
    /// The box: app stopped, old files deleted, old partitions dropped.
    Box,
    /// Every version of the old market-data copies in the cold bucket.
    S3,
    /// The old-root snapshot of the 2026-10-22 rebuild.
    Snapshot,
}

impl Phase {
    /// Parses a `--phase` word: one phase, or `cloud` (S3 then snapshot).
    #[must_use]
    pub fn parse_list(s: &str) -> Option<Vec<Self>> {
        match s {
            "preflight" => Some(vec![Self::Preflight]),
            "box" => Some(vec![Self::Box]),
            "s3" => Some(vec![Self::S3]),
            "snapshot" => Some(vec![Self::Snapshot]),
            "cloud" => Some(vec![Self::S3, Self::Snapshot]),
            _ => None,
        }
    }

    /// Wire word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Box => "box",
            Self::S3 => "s3",
            Self::Snapshot => "snapshot",
        }
    }
}

/// What the wipe does with one key, file, partition or snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// In scope and dated before the cutoff: deleted.
    Delete,
    /// In scope but dated 2026-10-28 or later: kept.
    KeepNotOld,
    /// On the keep-list (SEBI / audit / deploys / logs / sebi-preserve /
    /// state files): kept, whatever its date.
    KeepProtected,
    /// Not a recognised shape: kept and reported.
    KeepUnparsed,
}

/// IST as a fixed offset.
fn ist() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(IST_OFFSET_SECS).unwrap_or_else(|| chrono::Offset::fix(&Utc))
}

fn cutoff_date() -> Option<NaiveDate> {
    NaiveDate::parse_from_str(CUTOFF_IST_DATE, "%Y-%m-%d").ok()
}

/// The cutoff instant: 2026-10-28 00:00 IST, i.e. 2026-10-27 18:30 UTC, as
/// Unix seconds. `None` only if the constant does not parse (tested).
#[must_use]
pub fn cutoff_unix_secs() -> Option<i64> {
    let d = cutoff_date()?;
    ist()
        .from_local_datetime(&d.and_time(NaiveTime::MIN))
        .single()
        .map(|t| t.timestamp())
}

/// The SEBI / audit keep-list: every DAY-partitioned audit table and every
/// retention-exempt table, exactly the list the 2026-10-22 rebuild carried
/// onto the new disk (`partition_manager::rebuild_keep_tables`). A table on
/// it is never dropped and its S3 copies are never deleted.
#[must_use]
pub fn is_keep_table(name: &str) -> bool {
    tickvault_storage::partition_manager::rebuild_keep_tables().contains(&name)
}

/// The market-data tables Quote 35 deletes, derived from the code's own
/// table registry, never restated: every table retention manages, minus the
/// keep-list (what is left is the HOUR-partitioned market-data tables and
/// every candle table, including the archive-only 3 s / 5 s ones), plus the
/// retired candle tables whose old S3 copies may still exist. Sorted,
/// de-duplicated, and never containing a keep-list table.
#[must_use]
pub fn market_data_tables() -> Vec<&'static str> {
    let mut out: Vec<&'static str> =
        tickvault_storage::partition_manager::all_managed_table_names();
    out.extend(tickvault_storage::shadow_persistence::retired_candle_table_names());
    out.retain(|t| !is_keep_table(t));
    out.sort_unstable();
    out.dedup();
    out
}

/// The exact prefixes the S3 phase lists, one per market-data table, the
/// dated roots, and the rebuild's whole-table root (listed once; every key
/// under it is classified by table, so a keep-list or unknown table there is
/// kept). The IAM `s3:prefix` condition allows these and no others.
#[must_use]
pub fn s3_list_prefixes() -> Vec<String> {
    let mut out: Vec<String> = market_data_tables()
        .iter()
        .map(|t| format!("{PARTITIONS_ROOT}{t}/"))
        .collect();
    out.extend(DATED_ROOTS.iter().map(|r| (*r).to_string()));
    out.push(REBUILD_TABLES_ROOT.to_string());
    out
}

/// `true` for `<64 lowercase hex>.csv.gz`, the only file name
/// `partition_archive::rebuild_whole_table_key` writes.
fn is_rebuild_whole_table_file(file: &str) -> bool {
    file.strip_suffix(".csv.gz").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// One object under [`REBUILD_TABLES_ROOT`]: deleted only when its table is
/// market data (never a keep-list table, e.g. `top_volume_rank`), its file
/// name is the rebuild's content-addressed shape, and the version was last
/// written before the cutoff instant. The key carries no date, so the
/// version's own last-modified time dates it; an unknown time keeps it.
fn rebuild_table_verdict(rest: &str, last_modified_unix_secs: Option<i64>) -> Verdict {
    let Some((table, file)) = rest.split_once('/') else {
        return Verdict::KeepUnparsed;
    };
    if is_keep_table(table) {
        return Verdict::KeepProtected;
    }
    if !market_data_tables().contains(&table) || !is_rebuild_whole_table_file(file) {
        return Verdict::KeepUnparsed;
    }
    match (last_modified_unix_secs, cutoff_unix_secs()) {
        (Some(t), Some(c)) if t < c => Verdict::Delete,
        (Some(_), Some(_)) => Verdict::KeepNotOld,
        _ => Verdict::KeepUnparsed,
    }
}

/// A real `YYYY-MM-DD` date at the start of `s`.
fn leading_date(s: &str) -> Option<NaiveDate> {
    let head = s.get(..10)?;
    let shape_ok = head.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()
}

fn date_verdict(date: NaiveDate) -> Verdict {
    if date.format("%Y").to_string() != GRANTED_YEAR {
        return Verdict::KeepUnparsed;
    }
    match cutoff_date() {
        Some(c) if date < c => Verdict::Delete,
        Some(_) => Verdict::KeepNotOld,
        None => Verdict::KeepUnparsed,
    }
}

/// One partition of a market-data table, by its QuestDB partition name
/// (`2026-10-01`, `2026-10-01T09`, with an optional `.<suffix>` as the
/// archiver's object names and QuestDB's version suffixes carry). The
/// designated `ts` of these tables is IST wall time (the WebSocket rule:
/// never shifted), so the name's date is the IST date.
///
/// A keep-list table is [`Verdict::KeepProtected`] whatever its name; a
/// table that is not market data is [`Verdict::KeepUnparsed`].
#[must_use]
pub fn partition_verdict(table: &str, name: &str) -> Verdict {
    if is_keep_table(table) {
        return Verdict::KeepProtected;
    }
    if !market_data_tables().contains(&table) {
        return Verdict::KeepUnparsed;
    }
    let Some(date) = leading_date(name) else {
        return Verdict::KeepUnparsed;
    };
    match name.as_bytes().get(10) {
        None | Some(b'.') | Some(b'T') => {}
        Some(_) => return Verdict::KeepUnparsed,
    }
    if name.contains('/') || name.contains('\'') {
        return Verdict::KeepUnparsed;
    }
    date_verdict(date)
}

/// Classifies one object version of the cold bucket by its key and, for the
/// undated rebuild keys only, its last-modified time (Unix seconds).
///
/// Any key naming `audit` or `sebi` anywhere is kept, whatever its root
/// (defence in depth: the box classifier keeps such paths the same way).
#[must_use]
pub fn classify_s3_key(key: &str, last_modified_unix_secs: Option<i64>) -> Verdict {
    if PROTECTED_ROOTS.iter().any(|r| key.starts_with(r))
        || key.starts_with(REBUILD_KEEP_ROOT)
        || key.contains("audit")
        || key.contains("sebi")
    {
        return Verdict::KeepProtected;
    }
    if let Some(rest) = key.strip_prefix(REBUILD_TABLES_ROOT) {
        return rebuild_table_verdict(rest, last_modified_unix_secs);
    }
    if let Some(rest) = key.strip_prefix(PARTITIONS_ROOT) {
        let Some((table, name)) = rest.split_once('/') else {
            return Verdict::KeepUnparsed;
        };
        if is_keep_table(table) {
            return Verdict::KeepProtected;
        }
        if name.is_empty() || name.contains('/') {
            return Verdict::KeepUnparsed;
        }
        return partition_verdict(table, name);
    }
    for root in DATED_ROOTS {
        if let Some(rest) = key.strip_prefix(root) {
            let Some((dir, file)) = rest.split_once('/') else {
                return Verdict::KeepUnparsed;
            };
            if dir.len() != 10 || file.is_empty() {
                return Verdict::KeepUnparsed;
            }
            return leading_date(dir).map_or(Verdict::KeepUnparsed, date_verdict);
        }
    }
    Verdict::KeepUnparsed
}

/// `true` for a WAL segment file name (`ws-frames-<digits>.wal`).
fn is_wal_segment_name(name: &str) -> bool {
    name.strip_prefix("ws-frames-")
        .and_then(|r| r.strip_suffix(".wal"))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// One file on the box, by its path relative to `/opt/tickvault` and its
/// last-write time (Unix seconds).
///
/// | Path | Verdict |
/// |---|---|
/// | `data/ws_wal/**/ws-frames-<n>.wal` | by date (the frame log) |
/// | any other file under `data/ws_wal/` | kept (WAL state: watermark, ranges, owner lock, upload markers) |
/// | `data/spill/audit/**` | kept (SEBI audit rows waiting to be written) |
/// | any other file under `data/spill/` or `data/dlq/` | by date (tick, depth, seal, top_volume spill, seal DLQ) |
/// | anything with an `audit` or `sebi-preserve` component, `..`, or elsewhere | kept |
///
/// "By date": deleted when last written before 2026-10-28 00:00 IST.
#[must_use]
pub fn box_file_verdict(rel: &str, mtime_unix_secs: i64) -> Verdict {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Verdict::KeepUnparsed;
    }
    if parts
        .iter()
        .any(|p| *p == "audit" || *p == "sebi-preserve" || p.contains("audit"))
    {
        return Verdict::KeepProtected;
    }
    let file = parts.last().copied().unwrap_or_default();
    let in_scope = match parts.as_slice() {
        ["data", "ws_wal", .., _] => {
            if !is_wal_segment_name(file) {
                return Verdict::KeepProtected;
            }
            true
        }
        ["data", "spill", .., _] | ["data", "dlq", .., _] => true,
        _ => false,
    };
    if !in_scope || parts.len() < 3 {
        return Verdict::KeepUnparsed;
    }
    match cutoff_unix_secs() {
        Some(c) if mtime_unix_secs < c => Verdict::Delete,
        Some(_) => Verdict::KeepNotOld,
        None => Verdict::KeepUnparsed,
    }
}

/// One entry inside a QuestDB table directory (`<table>` or `<table>~<n>`):
/// only a `<partition>.detached` directory of a market-data table, dated
/// before the cutoff, is deleted. Everything else QuestDB owns is kept.
#[must_use]
pub fn detached_dir_verdict(table_dir: &str, entry: &str) -> Verdict {
    let table = table_dir.split_once('~').map_or(table_dir, |(t, n)| {
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
            t
        } else {
            table_dir
        }
    });
    if is_keep_table(table) {
        return Verdict::KeepProtected;
    }
    let Some(stem) = entry.strip_suffix(".detached") else {
        return Verdict::KeepUnparsed;
    };
    partition_verdict(table, stem)
}

/// One EBS snapshot: deleted only when it carries BOTH tags the rebuild put
/// on the old-root snapshot ([`SNAPSHOT_TAG_KEY`]=[`SNAPSHOT_TAG_VALUE`] and
/// `Name`=[`SNAPSHOT_NAME`]) and is owned by this account. Any other
/// snapshot is kept.
#[must_use]
pub fn snapshot_verdict(tags: &[(String, String)], owned_by_self: bool) -> Verdict {
    let has = |k: &str, v: &str| tags.iter().any(|(tk, tv)| tk == k && tv == v);
    if owned_by_self && has(SNAPSHOT_TAG_KEY, SNAPSHOT_TAG_VALUE) && has("Name", SNAPSHOT_NAME) {
        Verdict::Delete
    } else {
        Verdict::KeepProtected
    }
}

/// `true` only for the `systemctl is-active tickvault` answers that mean the
/// app is not running (`inactive`, `failed`). Anything else, including an
/// empty or unknown answer, refuses every box delete.
#[must_use]
pub fn app_state_permits_delete(state: &str) -> bool {
    matches!(state.trim(), "inactive" | "failed")
}

fn ist_naive(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").ok()
}

/// Why a run must not start, or `None` when it may. `now_ist` is the wall
/// clock in IST.
///
/// - Every run (dry included) is refused after [`LAST_RUN_IST_DATE`].
/// - A dry run (and the read-only preflight) may run any other time.
/// - An apply needs [`APPLY_CONFIRM`], must start inside
///   [`APPLY_FROM_IST`]..[`APPLY_UNTIL_IST`], and never inside 09:00–15:45 IST.
#[must_use]
pub fn run_refusal(
    phase: Phase,
    mode: Mode,
    confirm: &str,
    now_ist: NaiveDateTime,
) -> Option<String> {
    const UNEVALUABLE: &str = "internal: the run window cannot be evaluated";
    let (Some(last), Some(from), Some(until), Some(mkt_from), Some(mkt_until)) = (
        NaiveDate::parse_from_str(LAST_RUN_IST_DATE, "%Y-%m-%d").ok(),
        ist_naive(APPLY_FROM_IST),
        ist_naive(APPLY_UNTIL_IST),
        NaiveTime::from_hms_opt(MARKET_FROM_HHMM.0, MARKET_FROM_HHMM.1, 0),
        NaiveTime::from_hms_opt(MARKET_UNTIL_HHMM.0, MARKET_UNTIL_HHMM.1, 0),
    ) else {
        return Some(UNEVALUABLE.to_string());
    };
    if now_ist.date() > last {
        return Some(format!(
            "Quotes 35 and 42 are one run; this tool expired after {LAST_RUN_IST_DATE} IST"
        ));
    }
    if mode == Mode::Dry || phase == Phase::Preflight {
        return None;
    }
    if confirm != APPLY_CONFIRM {
        return Some(format!("apply needs --confirm {APPLY_CONFIRM}"));
    }
    let t = now_ist.time();
    if t >= mkt_from && t < mkt_until {
        return Some("apply is refused 09:00-15:45 IST".to_string());
    }
    if now_ist < from || now_ist > until {
        return Some(format!(
            "apply runs only from {APPLY_FROM_IST} to {APPLY_UNTIL_IST} IST (Quote 42)"
        ));
    }
    None
}

/// Why the old-root snapshot must not be deleted yet, or `None` when it may.
/// Quote 42: no earlier than [`SNAPSHOT_DELETE_FROM_IST`], after Wednesday's
/// first session on the new box ends, so a failed rebuild can still be undone
/// overnight. Checked on top of [`run_refusal`] for every snapshot delete.
#[must_use]
pub fn snapshot_delete_refusal(now_ist: NaiveDateTime) -> Option<String> {
    let Some(from) = ist_naive(SNAPSHOT_DELETE_FROM_IST) else {
        return Some("internal: the snapshot delete time cannot be evaluated".to_string());
    };
    if now_ist < from {
        return Some(format!(
            "the old-root snapshot is kept until {SNAPSHOT_DELETE_FROM_IST} IST (Quote 42: after Wednesday's first session on the new box, so a failed rebuild can still be undone); re-run phase=cloud mode=apply after that"
        ));
    }
    None
}

/// `true` for a 40-hex lowercase commit id (the only value the SSM command
/// splices in besides the mode word).
#[must_use]
pub fn is_commit_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `true` for a partition name the box may splice into `DROP PARTITION LIST`
/// (digits, `-`, `T`, `.` only; at most 32 characters).
#[must_use]
pub fn is_sql_safe_partition_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'T' || b == b'.')
}

/// What the box does with one market-data table, from its non-detached
/// partition names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableAction {
    /// On the keep-list or not market data: untouched.
    Untouched(Verdict),
    /// No partitions dated before the cutoff.
    Nothing,
    /// Every partition is proven dated before the cutoff: `TRUNCATE TABLE`
    /// (QuestDB cannot drop the active, newest partition).
    Truncate(usize),
    /// Some partitions are old, some are not or do not parse: drop exactly
    /// the old ones (the newest partition is then never in the list).
    DropList(Vec<String>),
}

/// Decides the [`TableAction`] for one table. O(partitions).
#[must_use]
pub fn table_action(table: &str, partitions: &[String]) -> TableAction {
    if is_keep_table(table) {
        return TableAction::Untouched(Verdict::KeepProtected);
    }
    if !market_data_tables().contains(&table) {
        return TableAction::Untouched(Verdict::KeepUnparsed);
    }
    let old: Vec<String> = partitions
        .iter()
        .filter(|p| partition_verdict(table, p) == Verdict::Delete && is_sql_safe_partition_name(p))
        .cloned()
        .collect();
    if old.is_empty() {
        TableAction::Nothing
    } else if old.len() == partitions.len() {
        TableAction::Truncate(old.len())
    } else {
        TableAction::DropList(old)
    }
}

/// One listed S3 version or delete marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionEntry {
    /// Object key.
    pub key: String,
    /// `"null"` for a version written before versioning was enabled.
    pub version_id: String,
    /// Size (0 for a delete marker).
    pub bytes: i64,
    /// A delete marker rather than a version.
    pub is_delete_marker: bool,
    /// Last-modified time (Unix seconds); dates the undated rebuild keys.
    pub last_modified_unix_secs: Option<i64>,
}

/// Totals for one group in the S3 report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupTotals {
    /// Versions.
    pub versions: usize,
    /// Delete markers.
    pub delete_markers: usize,
    /// Bytes of the versions.
    pub bytes: i64,
}

/// What an S3 run deletes and keeps.
#[derive(Debug, Clone, Default)]
pub struct S3Plan {
    /// Entries to delete by version id.
    pub delete: Vec<VersionEntry>,
    /// Per group (`questdb-partitions/<table>`, `raw-frames`, ...).
    pub groups: std::collections::BTreeMap<String, GroupTotals>,
    /// Kept: dated 2026-10-28 or later.
    pub kept_not_old: usize,
    /// Kept: on the keep-list.
    pub kept_protected: usize,
    /// Kept: not a recognised key (named in the report).
    pub kept_unparsed: Vec<String>,
}

fn group_of(key: &str) -> String {
    for root in [PARTITIONS_ROOT, REBUILD_TABLES_ROOT] {
        if let Some(rest) = key.strip_prefix(root) {
            let table = rest.split('/').next().unwrap_or_default();
            return format!("{root}{table}");
        }
    }
    key.split('/').next().unwrap_or_default().to_string()
}

impl S3Plan {
    /// Sorts every listed entry into delete / keep. O(entries).
    #[must_use]
    pub fn from_entries(entries: Vec<VersionEntry>) -> Self {
        let mut plan = Self::default();
        for e in entries {
            match classify_s3_key(&e.key, e.last_modified_unix_secs) {
                Verdict::Delete => {
                    let g = plan.groups.entry(group_of(&e.key)).or_default();
                    if e.is_delete_marker {
                        g.delete_markers += 1;
                    } else {
                        g.versions += 1;
                        g.bytes = g.bytes.saturating_add(e.bytes.max(0));
                    }
                    plan.delete.push(e);
                }
                Verdict::KeepNotOld => plan.kept_not_old += 1,
                Verdict::KeepProtected => plan.kept_protected += 1,
                Verdict::KeepUnparsed => plan.kept_unparsed.push(e.key),
            }
        }
        plan.kept_unparsed.sort();
        plan.kept_unparsed.dedup();
        plan
    }

    /// Plain-text report for the job log. At most 50 kept keys are named;
    /// the counts are always exact.
    #[must_use]
    pub fn render(&self, title: &str) -> String {
        const MAX_LISTED: usize = 50;
        let bytes: i64 = self.groups.values().map(|g| g.bytes).sum();
        let mut out = format!(
            "{title}\nbucket {COLD_BUCKET}, keys dated before {CUTOFF_IST_DATE}\n\ngroup | versions | delete markers | GB\n"
        );
        for (name, g) in &self.groups {
            out.push_str(&format!(
                "{name} | {} | {} | {:.2}\n",
                g.versions,
                g.delete_markers,
                g.bytes as f64 / 1e9
            ));
        }
        out.push_str(&format!(
            "\nto delete: {} entries, {:.2} GB\nkept, dated {CUTOFF_IST_DATE} or later: {}\nkept, keep-list: {}\nkept, not a recognised key: {}\n",
            self.delete.len(),
            bytes as f64 / 1e9,
            self.kept_not_old,
            self.kept_protected,
            self.kept_unparsed.len()
        ));
        for k in self.kept_unparsed.iter().take(MAX_LISTED) {
            out.push_str(&format!("  {k}\n"));
        }
        if self.kept_unparsed.len() > MAX_LISTED {
            out.push_str(&format!(
                "  ... and {} more\n",
                self.kept_unparsed.len() - MAX_LISTED
            ));
        }
        out
    }
}

/// The one report line a box run prints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxReport {
    /// `dry` / `apply`.
    pub mode: String,
    /// True only when every check passed.
    pub ok: bool,
    /// Why not, one line each (capped).
    pub problems: Vec<String>,
    /// How many problems there were in total.
    pub problems_total: usize,
    /// What it found or did (capped).
    pub lines: Vec<String>,
}

fn clip(line: String) -> String {
    if line.chars().count() <= REPORT_LINE_MAX_CHARS {
        return line;
    }
    let mut out: String = line.chars().take(REPORT_LINE_MAX_CHARS - 3).collect();
    out.push_str("...");
    out
}

impl BoxReport {
    /// A new report.
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode: mode.as_str().to_string(),
            ..Self::default()
        }
    }

    /// Records a problem (the report can then never be `ok`).
    pub fn problem(&mut self, line: impl Into<String>) {
        self.problems_total = self.problems_total.saturating_add(1);
        if self.problems.len() < REPORT_MAX_PROBLEMS {
            self.problems.push(clip(line.into()));
        }
    }

    /// Records a fact line.
    pub fn line(&mut self, line: impl Into<String>) {
        if self.lines.len() < REPORT_MAX_LINES {
            self.lines.push(clip(line.into()));
        }
    }

    /// Sets `ok` from the problem count and renders the one output line.
    #[must_use]
    pub fn finish(mut self) -> (Self, String) {
        self.ok = self.problems_total == 0;
        let json = serde_json::to_string(&self).unwrap_or_else(|_| String::from("{}"));
        (self, format!("{BOX_REPORT_PREFIX}{json}"))
    }
}

/// The LAST report line in a box run's output.
#[must_use]
pub fn parse_box_report(stdout: &str) -> Option<BoxReport> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(BOX_REPORT_PREFIX))
        .and_then(|json| serde_json::from_str(json).ok())
}

/// The IST wall clock now.
#[must_use]
pub fn now_ist(now: DateTime<Utc>) -> NaiveDateTime {
    now.with_timezone(&ist()).naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn secs(s: &str) -> i64 {
        ist()
            .from_local_datetime(&at(s))
            .single()
            .unwrap()
            .timestamp()
    }

    fn entry(key: &str, bytes: i64, marker: bool) -> VersionEntry {
        VersionEntry {
            key: key.to_string(),
            version_id: "v".to_string(),
            bytes,
            is_delete_marker: marker,
            last_modified_unix_secs: Some(secs("2026-10-27 20:00")),
        }
    }

    #[test]
    fn test_cutoff_unix_secs_is_midnight_ist_on_the_twenty_eighth_of_october() {
        // 2026-10-27T18:30:00Z (Quote 42; was 2026-11-01 under Quote 35).
        assert_eq!(cutoff_unix_secs(), Some(1_793_125_800));
        assert_eq!(cutoff_unix_secs(), Some(secs("2026-10-28 00:00")));
    }

    #[test]
    fn test_is_keep_table_covers_every_sebi_and_audit_table() {
        for t in tickvault_storage::partition_manager::rebuild_keep_tables() {
            assert!(is_keep_table(t), "{t}");
        }
        for t in [
            "instrument_lifecycle",
            "instrument_lifecycle_audit",
            "index_constituency",
            "order_audit",
            "pnl_audit",
            "order_leg_pnl",
            "partition_archive_audit",
            "ws_event_audit",
            "schema_reset_log",
        ] {
            assert!(is_keep_table(t), "{t} must be on the keep-list");
        }
        assert!(!is_keep_table("ticks"));
    }

    #[test]
    fn test_market_data_tables_is_derived_sorted_and_never_holds_a_keep_table() {
        let t = market_data_tables();
        let mut sorted = t.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(t, sorted);
        for name in [
            "ticks",
            "market_depth",
            "market_depth_book",
            "feed_aux_packets",
            "candles_1s",
            "candles_1m",
            "candles_3m",
            "candles_5m",
            "candles_10m",
            "candles_15m",
            "candles_30m",
            "candles_60m",
            "candles_3s",
            "candles_5s",
            "candles_1d",
            "top_volume_1s",
            "top_volume_3s",
            "top_volume_5s",
            "top_volume_1m",
        ] {
            assert!(t.contains(&name), "{name} must be wiped");
        }
        for name in &t {
            assert!(!is_keep_table(name), "{name}");
            assert!(
                !name.contains("audit") && !name.contains("lifecycle"),
                "{name}"
            );
            assert!(
                !name.contains("constituency") && !name.contains("order"),
                "{name}"
            );
        }
    }

    #[test]
    fn test_market_data_tables_match_the_iam_grant() {
        let tf = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/aws/terraform/wipe-2026-10-31.tf"
        ))
        .unwrap();
        let start = tf.find("wipe_tables = [").unwrap();
        let block = &tf[start..start + tf[start..].find(']').unwrap()];
        let in_tf: Vec<&str> = block.split('"').skip(1).step_by(2).collect();
        assert_eq!(in_tf, market_data_tables());
        // The snapshot tags the role may delete are the ones this module keys on.
        assert!(tf.contains(&format!(
            "\"aws:ResourceTag/{SNAPSHOT_TAG_KEY}\" = \"{SNAPSHOT_TAG_VALUE}\""
        )));
        assert!(tf.contains(&format!(
            "\"aws:ResourceTag/Name\"       = \"{SNAPSHOT_NAME}\""
        )));
        // Never a delete-marker grant, never a bucket-wide delete.
        assert!(!tf.contains("\"s3:DeleteObject\""));
        assert!(!tf.contains("tv_cold.arn}/*\""));
    }

    #[test]
    fn test_s3_list_prefixes_are_exact_and_never_protected() {
        let p = s3_list_prefixes();
        assert_eq!(p.len(), market_data_tables().len() + DATED_ROOTS.len() + 1);
        assert!(p.iter().all(|x| x.ends_with('/')));
        for want in [
            "questdb-partitions/ticks/",
            "raw-frames/",
            "seal-spill/",
            "top-volume-spill/",
            "tick-quarantine/",
            "depth-quarantine/",
            "top-volume-quarantine/",
            "rebuild-2026-10-22/tables/",
        ] {
            assert!(p.contains(&want.to_string()), "{want}");
        }
        for x in &p {
            assert!(!PROTECTED_ROOTS.iter().any(|r| x.starts_with(r)), "{x}");
            if let Some(t) = x.strip_prefix(PARTITIONS_ROOT) {
                assert!(!is_keep_table(t.trim_end_matches('/')), "{x}");
            }
        }
    }

    #[test]
    fn test_classify_s3_key_refuses_every_keep_table_and_protected_root() {
        for t in tickvault_storage::partition_manager::rebuild_keep_tables() {
            let key = format!("questdb-partitions/{t}/2026-10-01.csv.gz");
            assert_eq!(classify_s3_key(&key, None), Verdict::KeepProtected, "{key}");
        }
        for k in [
            "deploys/abc/tickvault",
            "deploys/2026-10-01/x",
            "sebi-preserve/2026-10-01/x.csv",
            "logs/2026/10/01/app.gz",
            "logs/raw-frames/2026-10-01/x",
        ] {
            assert_eq!(classify_s3_key(k, None), Verdict::KeepProtected, "{k}");
        }
    }

    #[test]
    fn test_classify_s3_key_date_fence_and_shapes() {
        use Verdict::*;
        let cases = [
            ("questdb-partitions/ticks/2026-10-27T23.csv.gz", Delete),
            ("questdb-partitions/ticks/2026-10-27.csv.gz", Delete),
            (
                "questdb-partitions/ticks/2026-10-01T09.sha-ab12.csv.gz",
                Delete,
            ),
            ("questdb-partitions/candles_3s/2026-10-20.csv.gz", Delete),
            (
                "questdb-partitions/top_volume_1m/2026-10-02T10.csv.gz",
                Delete,
            ),
            ("questdb-partitions/ticks/2026-10-28.csv.gz", KeepNotOld),
            ("questdb-partitions/ticks/2026-10-28T00.csv.gz", KeepNotOld),
            ("questdb-partitions/ticks/2027-01-02.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/2025-12-31.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/2026-02-30.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/x2026-10-01.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/2026-10-01x.csv.gz", KeepUnparsed),
            (
                "questdb-partitions/ticks/sub/2026-10-01.csv.gz",
                KeepUnparsed,
            ),
            ("questdb-partitions/ticks_x/2026-10-01.csv.gz", KeepUnparsed),
            ("questdb-partitions/ticks/", KeepUnparsed),
            ("questdb-partitions/ticks", KeepUnparsed),
            ("raw-frames/2026-10-27/ws-frames-1.wal.gz", Delete),
            ("seal-spill/2026-10-01/seals_v6-x.bin.gz", Delete),
            ("top-volume-spill/2026-10-01/x.gz", Delete),
            ("raw-frames/2026-10-28/ws-frames-1.wal.gz", KeepNotOld),
            ("seal-spill/2026-10-29/x", KeepNotOld),
            ("raw-frames/2026-10-08/", KeepUnparsed),
            ("raw-frames/2026-10-8/x", KeepUnparsed),
            ("raw-frames/latest/x", KeepUnparsed),
            ("seal-spill/x.bin", KeepUnparsed),
            ("tick-quarantine/2026-10-01/x.gz", Delete),
            ("rebuild-2026-10-22/files/data/x", KeepUnparsed),
            ("", KeepUnparsed),
        ];
        for (k, want) in cases {
            assert_eq!(classify_s3_key(k, None), want, "{k}");
        }
    }

    #[test]
    fn test_partition_verdict_names_and_tables() {
        use Verdict::*;
        assert_eq!(partition_verdict("ticks", "2026-10-27T23"), Delete);
        assert_eq!(partition_verdict("ticks", "2026-10-27"), Delete);
        assert_eq!(partition_verdict("ticks", "2026-10-27T23.12"), Delete);
        assert_eq!(partition_verdict("ticks", "2026-10-28T00"), KeepNotOld);
        assert_eq!(partition_verdict("ticks", "2026-11-05"), KeepNotOld);
        assert_eq!(partition_verdict("ticks", "2026-10-27X"), KeepUnparsed);
        assert_eq!(partition_verdict("ticks", "garbage"), KeepUnparsed);
        assert_eq!(
            partition_verdict("ticks", "2026-10-01'; DROP"),
            KeepUnparsed
        );
        assert_eq!(
            partition_verdict("order_audit", "2026-10-01"),
            KeepProtected
        );
        assert_eq!(
            partition_verdict("something_new", "2026-10-01"),
            KeepUnparsed
        );
    }

    #[test]
    fn test_box_file_verdict_scope_keep_list_and_date_fence() {
        use Verdict::*;
        let old = secs("2026-10-27 23:59");
        let new = secs("2026-10-28 00:00");
        let cases = [
            (
                "data/ws_wal/ws-frames-01790000000000000000.wal",
                old,
                Delete,
            ),
            ("data/ws_wal/replaying/ws-frames-17.wal", old, Delete),
            ("data/ws_wal/archive/ws-frames-17.wal", old, Delete),
            ("data/ws_wal/ws-frames-17.wal", new, KeepNotOld),
            ("data/ws_wal/applied.tvaw", old, KeepProtected),
            ("data/ws_wal/unapplied_ranges.tvur", old, KeepProtected),
            ("data/ws_wal/deferred_depth.tvdd", old, KeepProtected),
            ("data/ws_wal/.wal-owner.lock", old, KeepProtected),
            (
                "data/ws_wal/markers/ws-frames-17.wal.uploaded",
                old,
                KeepProtected,
            ),
            ("data/spill/ticks/ticks-2026-10-30.ilp", old, Delete),
            ("data/spill/depth/x.ilp", old, Delete),
            ("data/spill/top_volume/x", old, Delete),
            ("data/spill/seals_v6-2026-10-30.bin", old, Delete),
            ("data/spill/replaying/seals_v6-2026-10-30.bin", old, Delete),
            ("data/dlq/seals-2026-10-30.ndjson", old, Delete),
            ("data/spill/ticks/x.ilp", new, KeepNotOld),
            ("data/spill/audit/order_audit/x.ilp", old, KeepProtected),
            ("data/spill/audit/pnl_audit/x.ilp", old, KeepProtected),
            ("data/spill/order_audit_x.ilp", old, KeepProtected),
            ("data/sebi-preserve/x.csv", old, KeepProtected),
            ("data/spill/sebi-preserve/x", old, KeepProtected),
            ("data/instruments/cache.rkyv", old, KeepUnparsed),
            ("data/state/daily/x.marker", old, KeepUnparsed),
            ("data/logs/machine/app.log", old, KeepUnparsed),
            ("config/base.toml", old, KeepUnparsed),
            ("data/spill/../state/x", old, KeepUnparsed),
            ("data/spill//x", old, KeepUnparsed),
            ("data/spill", old, KeepUnparsed),
            ("", old, KeepUnparsed),
        ];
        for (rel, t, want) in cases {
            assert_eq!(box_file_verdict(rel, t), want, "{rel}");
        }
    }

    #[test]
    fn test_detached_dir_verdict_only_old_market_data_detached_dirs() {
        use Verdict::*;
        assert_eq!(
            detached_dir_verdict("ticks~12", "2026-10-01T09.detached"),
            Delete
        );
        assert_eq!(
            detached_dir_verdict("ticks", "2026-10-01.3.detached"),
            Delete
        );
        assert_eq!(
            detached_dir_verdict("ticks~12", "2026-10-28T09.detached"),
            KeepNotOld
        );
        assert_eq!(
            detached_dir_verdict("ticks~12", "2026-10-01T09"),
            KeepUnparsed
        );
        assert_eq!(detached_dir_verdict("ticks~12", "_meta"), KeepUnparsed);
        assert_eq!(
            detached_dir_verdict("order_audit~3", "2026-10-01.detached"),
            KeepProtected
        );
        assert_eq!(
            detached_dir_verdict("instrument_lifecycle", "1970-01-01.detached"),
            KeepProtected
        );
        assert_eq!(
            detached_dir_verdict("ticks~x", "2026-10-01.detached"),
            KeepUnparsed
        );
    }

    #[test]
    fn test_snapshot_verdict_needs_both_tags_and_ownership() {
        let tags = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };
        let both = tags(&[
            (SNAPSHOT_TAG_KEY, SNAPSHOT_TAG_VALUE),
            ("Name", SNAPSHOT_NAME),
        ]);
        assert_eq!(snapshot_verdict(&both, true), Verdict::Delete);
        assert_eq!(snapshot_verdict(&both, false), Verdict::KeepProtected);
        assert_eq!(
            snapshot_verdict(&tags(&[(SNAPSHOT_TAG_KEY, SNAPSHOT_TAG_VALUE)]), true),
            Verdict::KeepProtected
        );
        assert_eq!(
            snapshot_verdict(&tags(&[("Name", SNAPSHOT_NAME)]), true),
            Verdict::KeepProtected
        );
        assert_eq!(
            snapshot_verdict(
                &tags(&[(SNAPSHOT_TAG_KEY, "2026-10-23"), ("Name", SNAPSHOT_NAME)]),
                true
            ),
            Verdict::KeepProtected
        );
        assert_eq!(snapshot_verdict(&[], true), Verdict::KeepProtected);
    }

    #[test]
    fn test_app_state_permits_delete_only_when_stopped() {
        assert!(app_state_permits_delete("inactive"));
        assert!(app_state_permits_delete("failed\n"));
        for s in [
            "active",
            "activating",
            "deactivating",
            "reloading",
            "",
            "unknown",
        ] {
            assert!(!app_state_permits_delete(s), "{s}");
        }
    }

    #[test]
    fn test_run_refusal_window_confirm_and_expiry() {
        use Mode::*;
        use Phase::*;
        // Dry runs any hour until the last day, market hours included.
        assert_eq!(run_refusal(Box, Dry, "", at("2026-10-23 10:00")), None);
        assert_eq!(run_refusal(S3, Dry, "", at("2026-11-01 23:59")), None);
        assert!(run_refusal(Box, Dry, "", at("2026-11-02 00:00")).is_some());
        assert!(run_refusal(Preflight, Apply, "", at("2026-11-02 00:00")).is_some());
        assert_eq!(
            run_refusal(Preflight, Apply, "", at("2026-10-23 10:00")),
            None
        );
        // Apply needs the word.
        assert!(run_refusal(Box, Apply, "", at("2026-10-27 22:00")).is_some());
        assert!(run_refusal(Box, Apply, "wipe", at("2026-10-27 22:00")).is_some());
        // Apply from 18:00 IST on Tuesday 27 Oct (Quote 42) to 08:00 IST
        // on 1 Nov, never in market hours.
        for ok in [
            "2026-10-27 18:00",
            "2026-10-27 23:59",
            "2026-10-28 03:00",
            "2026-10-28 16:30",
            "2026-10-31 18:00",
            "2026-11-01 03:00",
            "2026-11-01 08:00",
        ] {
            assert_eq!(run_refusal(Box, Apply, APPLY_CONFIRM, at(ok)), None, "{ok}");
            assert_eq!(run_refusal(S3, Apply, APPLY_CONFIRM, at(ok)), None, "{ok}");
            assert_eq!(
                run_refusal(Snapshot, Apply, APPLY_CONFIRM, at(ok)),
                None,
                "{ok}"
            );
        }
        for bad in [
            "2026-10-23 22:00",
            "2026-10-27 08:00",
            "2026-10-27 15:44",
            "2026-10-27 17:59",
            "2026-10-28 09:00",
            "2026-10-28 15:44",
            "2026-11-01 08:01",
            "2026-11-01 10:00",
            "2026-11-01 15:44",
            "2026-11-05 22:00",
        ] {
            assert!(
                run_refusal(Box, Apply, APPLY_CONFIRM, at(bad)).is_some(),
                "{bad}"
            );
        }
        // Never inside 09:00-15:45 IST (checked before the window).
        let r = run_refusal(Box, Apply, APPLY_CONFIRM, at("2026-10-28 09:00")).unwrap();
        assert!(r.contains("09:00-15:45"), "{r}");
    }

    #[test]
    fn test_snapshot_delete_refused_before_wednesday_close() {
        // Quote 42: the old-root snapshot stays until 2026-10-28 16:30 IST.
        for early in [
            "2026-10-27 18:00",
            "2026-10-27 23:59",
            "2026-10-28 08:00",
            "2026-10-28 16:29",
        ] {
            let why = snapshot_delete_refusal(at(early)).unwrap_or_default();
            assert!(why.contains(SNAPSHOT_DELETE_FROM_IST), "{early}: {why}");
            assert!(why.contains("Quote 42"), "{early}: {why}");
        }
        for ok in ["2026-10-28 16:30", "2026-10-28 22:00", "2026-11-01 08:00"] {
            assert_eq!(snapshot_delete_refusal(at(ok)), None, "{ok}");
        }
        // The run window never opens before the snapshot time is checked:
        // tonight the apply window is open but the snapshot is still refused.
        let tonight = at("2026-10-27 21:00");
        assert_eq!(
            run_refusal(Phase::Snapshot, Mode::Apply, APPLY_CONFIRM, tonight),
            None
        );
        assert!(snapshot_delete_refusal(tonight).is_some());
    }

    #[test]
    fn test_regression_wipe_never_touches_data_dated_2026_10_28_or_later() {
        use Verdict::*;
        // Partition keys (day, hour, version-suffixed, sha sidecar), dated
        // roots and the quarantine roots, all on Wednesday or later.
        for key in [
            "questdb-partitions/ticks/2026-10-28.csv.gz",
            "questdb-partitions/ticks/2026-10-28T00.csv.gz",
            "questdb-partitions/ticks/2026-10-28T09.sha-ab12.csv.gz",
            "questdb-partitions/candles_1m/2026-10-29.csv.gz",
            "questdb-partitions/market_depth/2026-10-31T15.csv.gz",
            "raw-frames/2026-10-28/ws-frames-1.wal.gz",
            "seal-spill/2026-10-28/seals_v6-x.bin.gz",
            "top-volume-spill/2026-10-30/x.gz",
            "tick-quarantine/2026-10-28/ticks-1.ndjson.gz",
            "depth-quarantine/2026-10-28/d.gz",
            "top-volume-quarantine/2026-11-01/x.gz",
        ] {
            assert_eq!(classify_s3_key(key, None), KeepNotOld, "{key}");
        }
        // The undated rebuild copy is dated by its write time.
        let hex = "a".repeat(64);
        let key = format!("{REBUILD_TABLES_ROOT}ticks/{hex}.csv.gz");
        assert_eq!(
            classify_s3_key(&key, Some(secs("2026-10-28 00:00"))),
            KeepNotOld
        );
        assert_eq!(classify_s3_key(&key, None), KeepUnparsed);
        assert_eq!(
            classify_s3_key(&key, Some(secs("2026-10-27 23:59"))),
            Delete
        );
        // Partitions and detached dirs on the box.
        assert_eq!(partition_verdict("ticks", "2026-10-28"), KeepNotOld);
        assert_eq!(partition_verdict("ticks", "2026-10-28T00"), KeepNotOld);
        assert_eq!(partition_verdict("ticks", "2026-10-27T23"), Delete);
        assert_eq!(
            detached_dir_verdict("ticks~1", "2026-10-28T00.detached"),
            KeepNotOld
        );
        let names = vec!["2026-10-27T15".to_string(), "2026-10-28T09".to_string()];
        assert_eq!(
            table_action("ticks", &names),
            TableAction::DropList(vec!["2026-10-27T15".to_string()])
        );
        // Files written on the new box from Wednesday's first second.
        let wed = secs("2026-10-28 00:00");
        for rel in [
            "data/ws_wal/ws-frames-1.wal",
            "data/spill/ticks/a.ilp",
            "data/dlq/seals.ndjson",
        ] {
            assert_eq!(box_file_verdict(rel, wed), KeepNotOld, "{rel}");
            assert_eq!(box_file_verdict(rel, wed - 1), Delete, "{rel}");
        }
    }

    #[test]
    fn test_rebuild_prefix_keeps_keep_list_tables() {
        use Verdict::*;
        // The rebuild's whole-table key shape, from the code that writes it.
        let hex = "0123456789abcdef".repeat(4);
        let tonight = Some(secs("2026-10-27 20:00"));
        let written = tickvault_storage::partition_archive::rebuild_whole_table_key(
            &format!(
                "{}/tables",
                tickvault_storage::rebuild_copy::REBUILD_S3_PREFIX
            ),
            "ticks",
            &hex,
        );
        assert!(written.starts_with(REBUILD_TABLES_ROOT), "{written}");
        assert_eq!(classify_s3_key(&written, tonight), Delete);
        // Every keep-list table under the rebuild root is kept (the rebuild
        // copies `top_volume_rank` there whole).
        for t in tickvault_storage::partition_manager::rebuild_keep_tables() {
            let key = format!("{REBUILD_TABLES_ROOT}{t}/{hex}.csv.gz");
            assert_eq!(classify_s3_key(&key, tonight), KeepProtected, "{key}");
        }
        let rank = format!("{REBUILD_TABLES_ROOT}top_volume_rank/{hex}.csv.gz");
        assert_eq!(classify_s3_key(&rank, tonight), KeepProtected);
        // The rebuild's keep copies, file copies and manifests are never deleted.
        for key in [
            format!("rebuild-2026-10-22/keep/order_audit/{hex}.csv.gz"),
            format!("rebuild-2026-10-22/keep/ticks/{hex}.csv.gz"),
            format!("rebuild-2026-10-22/files/spill/ticks/a.ilp.sha-{hex}"),
            "rebuild-2026-10-22/manifests/copy-20261027T133500Z.json".to_string(),
        ] {
            assert_ne!(classify_s3_key(&key, tonight), Delete, "{key}");
        }
        // Unknown tables and odd file names are kept and reported.
        for key in [
            format!("{REBUILD_TABLES_ROOT}not_a_table/{hex}.csv.gz"),
            format!("{REBUILD_TABLES_ROOT}ticks/{hex}.csv"),
            format!("{REBUILD_TABLES_ROOT}ticks/ABC.csv.gz"),
            format!("{REBUILD_TABLES_ROOT}ticks/sub/{hex}.csv.gz"),
            format!("{REBUILD_TABLES_ROOT}ticks"),
        ] {
            assert_eq!(classify_s3_key(&key, tonight), KeepUnparsed, "{key}");
        }
        assert!(s3_list_prefixes().contains(&REBUILD_TABLES_ROOT.to_string()));
    }

    /// IAM's ARN wildcard match: `*` any run (including `/`), `?` one char.
    fn iam_glob(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => iam_glob(&p[1..], s) || (!s.is_empty() && iam_glob(p, &s[1..])),
            (Some(b'?'), Some(_)) => iam_glob(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => iam_glob(&p[1..], &s[1..]),
            _ => false,
        }
    }

    /// The S3 delete patterns the terraform grants and denies, as the tool
    /// sees them (bucket ARN stripped). Kept in lockstep with
    /// `wipe-2026-10-31.tf` by `test_iam_patterns_are_the_terraform_patterns`.
    fn iam_allow_patterns() -> Vec<String> {
        let mut out = Vec::new();
        for t in market_data_tables() {
            out.push(format!("questdb-partitions/{t}/2026-??-??*"));
            out.push(format!("{REBUILD_TABLES_ROOT}{t}/*"));
        }
        for r in DATED_ROOTS {
            out.push(format!("{r}2026-??-??/*"));
        }
        out
    }

    const IAM_DENY_DATES: [&str; 5] = [
        "2026-10-28",
        "2026-10-29",
        "2026-10-3?",
        "2026-11-",
        "2026-12-",
    ];

    fn iam_deny_patterns() -> Vec<String> {
        let mut out: Vec<String> = IAM_DENY_DATES.iter().map(|d| format!("*/{d}*")).collect();
        for r in PROTECTED_ROOTS {
            out.push(format!("{r}*"));
        }
        out.push(format!("{REBUILD_KEEP_ROOT}*"));
        out
    }

    #[test]
    fn test_iam_patterns_are_the_terraform_patterns() {
        let tf = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/aws/terraform/wipe-2026-10-31.tf"
        ))
        .unwrap();
        // Quote 42 dates: deletes from 18:00 IST Tuesday, the snapshot from
        // 16:30 IST Wednesday, expiry unchanged.
        assert!(tf.contains("wipe_deletes_from = \"2026-10-27T12:30:00Z\""));
        assert!(tf.contains("wipe_snapshot_delete_from = \"2026-10-28T11:00:00Z\""));
        assert!(tf.contains("wipe_expires_at = \"2026-11-01T18:30:00Z\""));
        assert!(tf.contains("\"aws:CurrentTime\" = local.wipe_snapshot_delete_from"));
        for want in [
            "/questdb-partitions/${t}/2026-??-??*\"",
            "/rebuild-2026-10-22/tables/${t}/*\"",
            "/${r}/2026-??-??/*\"",
            "/*/${d}*\"",
            "/rebuild-2026-10-22/keep/*\"",
            "[\"rebuild-2026-10-22/tables/\"]",
        ] {
            assert!(tf.contains(want), "{want}");
        }
        for r in DATED_ROOTS {
            let quoted = format!("\"{}\"", r.trim_end_matches('/'));
            assert!(tf.contains(&quoted), "{r}");
        }
        let deny = IAM_DENY_DATES.map(|d| format!("\"{d}\"")).join(", ");
        assert!(
            tf.contains(&format!("wipe_deny_dates = [{deny}]")),
            "{deny}"
        );
        for r in PROTECTED_ROOTS {
            assert!(tf.contains(&format!("tv_cold.arn}}/{r}*\"")), "{r}");
        }
    }

    #[test]
    fn test_every_delete_is_granted_and_nothing_denied_is_a_delete() {
        let allow = iam_allow_patterns();
        let deny = iam_deny_patterns();
        let hex = "f".repeat(64);
        let tonight = Some(secs("2026-10-27 20:00"));
        let mut keys = Vec::new();
        for t in market_data_tables() {
            for d in ["2026-09-30", "2026-10-01", "2026-10-27", "2026-10-28"] {
                keys.push(format!("questdb-partitions/{t}/{d}.csv.gz"));
                keys.push(format!("questdb-partitions/{t}/{d}T09.sha-{hex}.csv.gz"));
            }
            keys.push(format!("{REBUILD_TABLES_ROOT}{t}/{hex}.csv.gz"));
        }
        for r in DATED_ROOTS {
            for d in ["2026-09-30", "2026-10-27", "2026-10-28", "2026-10-31"] {
                keys.push(format!("{r}{d}/x.gz"));
            }
        }
        let mut deletes = 0;
        for k in &keys {
            let granted = allow.iter().any(|p| iam_glob(p.as_bytes(), k.as_bytes()));
            let denied = deny.iter().any(|p| iam_glob(p.as_bytes(), k.as_bytes()));
            if classify_s3_key(k, tonight) == Verdict::Delete {
                deletes += 1;
                assert!(granted && !denied, "{k}: granted {granted} denied {denied}");
            } else {
                assert!(
                    !granted || denied || k.starts_with(REBUILD_TABLES_ROOT),
                    "{k}"
                );
            }
        }
        assert!(deletes > 100, "{deletes}");
    }

    #[test]
    fn test_snapshot_delete_refusal_is_none_from_wednesday_close_to_expiry() {
        assert_eq!(snapshot_delete_refusal(at(SNAPSHOT_DELETE_FROM_IST)), None);
        let one_minute_early = at(SNAPSHOT_DELETE_FROM_IST) - chrono::TimeDelta::minutes(1);
        assert!(snapshot_delete_refusal(one_minute_early).is_some());
        assert_eq!(snapshot_delete_refusal(at(APPLY_UNTIL_IST)), None);
    }

    /// IAM refuses a role's inline policies past 10,240 characters
    /// (whitespace not counted). The policy body in the terraform, stripped of
    /// whitespace, bounds the fixed statements; every ARN and list prefix the
    /// `for` expressions render is added on top. An estimate (HCL and JSON
    /// differ by a few characters a line), held well under the limit.
    #[test]
    fn test_iam_policy_fits_the_inline_limit() {
        const IAM_ROLE_INLINE_LIMIT: usize = 10_240;
        const BUCKET_ARN: &str = "arn:aws:s3:::tv-prod-cold/";
        let tf = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/aws/terraform/wipe-2026-10-31.tf"
        ))
        .unwrap();
        let start = tf.find("resource \"aws_iam_role_policy\"").unwrap();
        let end = start + tf[start..].find("\noutput ").unwrap();
        let fixed = tf[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .flat_map(str::chars)
            .filter(|c| !c.is_whitespace())
            .count();
        let rendered: usize = iam_allow_patterns()
            .iter()
            .chain(iam_deny_patterns().iter())
            .map(|p| BUCKET_ARN.len() + p.len() + 3)
            .chain(s3_list_prefixes().iter().map(|p| p.len() + 3))
            .sum();
        let total = fixed + rendered;
        assert!(
            total < IAM_ROLE_INLINE_LIMIT - 1_000,
            "estimated inline policy {total} chars (fixed {fixed}, rendered {rendered})"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2000))]
        /// Nothing on the keep-list, and nothing dated 2026-10-28 or later,
        /// is ever a Delete, for any date, table, root or write time.
        #[test]
        fn proptest_keep_list_and_wednesday_onwards_are_never_deleted(
            year in 2025i32..=2027,
            month in 1u32..=12,
            day in 1u32..=31,
            hour in proptest::option::of(0u32..=23),
            table_pick in 0usize..10_000,
            root_pick in 0usize..12,
            written_offset in -900_000i64..900_000,
        ) {
            use Verdict::*;
            let date = format!("{year:04}-{month:02}-{day:02}");
            let hour = hour.map_or(String::new(), |h| format!("T{h:02}"));
            let keep = tickvault_storage::partition_manager::rebuild_keep_tables();
            let market = market_data_tables();
            let all: Vec<&str> = keep.iter().chain(market.iter()).copied().collect();
            let table = all[table_pick % all.len()];
            let written = secs("2026-10-28 00:00") + written_offset;
            let cutoff = NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
            let dated_new = NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_ok_and(|d| d >= cutoff);
            let key = match root_pick {
                0..=3 => format!("questdb-partitions/{table}/{date}{hour}.csv.gz"),
                4 => format!("{REBUILD_TABLES_ROOT}{table}/{}.csv.gz", "b".repeat(64)),
                5 => format!("rebuild-2026-10-22/keep/{table}/{}.csv.gz", "c".repeat(64)),
                r => format!("{}{date}/f.gz", DATED_ROOTS[(r - 6) % DATED_ROOTS.len()]),
            };
            let v = classify_s3_key(&key, Some(written));
            if keep.contains(&table) && (root_pick <= 5) {
                proptest::prop_assert_ne!(v, Delete, "{}", key);
            }
            if root_pick == 4 && written >= secs("2026-10-28 00:00") {
                proptest::prop_assert_ne!(v, Delete, "{}", key);
            }
            if root_pick != 4 && dated_new {
                proptest::prop_assert_ne!(v, Delete, "{}", key);
            }
            if root_pick <= 3 {
                let p = format!("{date}{hour}");
                proptest::prop_assert_eq!(partition_verdict(table, &p) == Delete, v == Delete);
            }
            for prefix in ["deploys/", "sebi-preserve/", "logs/"] {
                proptest::prop_assert_eq!(
                    classify_s3_key(&format!("{prefix}{date}/{table}"), Some(written)),
                    KeepProtected
                );
            }
            // The box: a file written at or after the cutoff instant stays.
            for rel in ["data/ws_wal/ws-frames-7.wal", "data/spill/ticks/a.ilp"] {
                let bv = box_file_verdict(rel, written);
                proptest::prop_assert_eq!(bv == Delete, written < secs("2026-10-28 00:00"));
            }
        }
    }

    #[test]
    fn test_is_commit_sha_only_forty_lowercase_hex() {
        assert!(is_commit_sha("d1b4835470000000000000000000000000000000"));
        assert!(!is_commit_sha("D1B4835470000000000000000000000000000000"));
        assert!(!is_commit_sha("d1b483547"));
        assert!(!is_commit_sha(
            "d1b483547000000000000000000000000000000; reboot"
        ));
    }

    #[test]
    fn test_is_sql_safe_partition_name_rejects_quotes_and_spaces() {
        assert!(is_sql_safe_partition_name("2026-10-01T09"));
        assert!(is_sql_safe_partition_name("2026-10-01.3"));
        assert!(!is_sql_safe_partition_name("2026-10-01' OR 1=1"));
        assert!(!is_sql_safe_partition_name("2026 10 01"));
        assert!(!is_sql_safe_partition_name(""));
        assert!(!is_sql_safe_partition_name(&"1".repeat(33)));
    }

    #[test]
    fn test_table_action_truncate_drop_list_and_refusals() {
        let names = |v: &[&str]| -> Vec<String> { v.iter().map(|s| (*s).to_string()).collect() };
        assert_eq!(
            table_action("ticks", &names(&["2026-10-26T09", "2026-10-27T10"])),
            TableAction::Truncate(2)
        );
        assert_eq!(
            table_action("ticks", &names(&["2026-10-27T10", "2026-10-28T09"])),
            TableAction::DropList(names(&["2026-10-27T10"]))
        );
        // An unparsable partition is never dropped and blocks the truncate.
        assert_eq!(
            table_action("ticks", &names(&["2026-10-27T10", "weird"])),
            TableAction::DropList(names(&["2026-10-27T10"]))
        );
        assert_eq!(
            table_action("ticks", &names(&["2026-10-28T09"])),
            TableAction::Nothing
        );
        assert_eq!(table_action("ticks", &[]), TableAction::Nothing);
        for t in tickvault_storage::partition_manager::rebuild_keep_tables() {
            assert_eq!(
                table_action(t, &names(&["2026-10-01"])),
                TableAction::Untouched(Verdict::KeepProtected),
                "{t}"
            );
        }
        assert_eq!(
            table_action("not_ours", &names(&["2026-10-01"])),
            TableAction::Untouched(Verdict::KeepUnparsed)
        );
    }

    #[test]
    fn test_s3_plan_from_entries_counts_and_keeps() {
        let plan = S3Plan::from_entries(vec![
            entry("questdb-partitions/ticks/2026-10-08.csv.gz", 100, false),
            entry("questdb-partitions/ticks/2026-10-08.csv.gz", 0, true),
            entry("questdb-partitions/ticks/2026-11-02.csv.gz", 50, false),
            entry("raw-frames/2026-10-01/a.wal.gz", 7, false),
            entry("raw-frames/2026-10-01/a.wal.gz", -3, false),
            entry("questdb-partitions/order_audit/2026-10-01.csv.gz", 9, false),
            entry("raw-frames/odd", 1, false),
            entry("raw-frames/odd", 1, false),
        ]);
        assert_eq!(plan.delete.len(), 4);
        assert_eq!(plan.kept_not_old, 1);
        assert_eq!(plan.kept_protected, 1);
        assert_eq!(plan.kept_unparsed, vec!["raw-frames/odd".to_string()]);
        let ticks = &plan.groups["questdb-partitions/ticks"];
        assert_eq!(
            (ticks.versions, ticks.delete_markers, ticks.bytes),
            (1, 1, 100)
        );
        let raw = &plan.groups["raw-frames"];
        assert_eq!((raw.versions, raw.bytes), (2, 7));
    }

    #[test]
    fn test_s3_plan_render_lists_groups_and_caps_kept_keys() {
        let mut entries = vec![entry("seal-spill/2026-10-01/a", 2_000_000_000, false)];
        for i in 0..60 {
            entries.push(entry(&format!("raw-frames/odd-{i}"), 1, false));
        }
        let r = S3Plan::from_entries(entries).render("DRY RUN");
        assert!(r.starts_with("DRY RUN\n"));
        assert!(r.contains("seal-spill | 1 | 0 | 2.00"));
        assert!(r.contains("kept, not a recognised key: 60"));
        assert!(r.contains("and 10 more"));
        let empty = S3Plan::from_entries(Vec::new()).render("t");
        assert!(empty.contains("to delete: 0 entries"));
    }

    #[test]
    fn test_box_report_finish_and_parse_box_report_round_trip() {
        let mut r = BoxReport::new(Mode::Dry);
        r.line("x".repeat(500));
        let (r, line) = r.finish();
        assert!(r.ok);
        assert!(r.lines[0].chars().count() <= REPORT_LINE_MAX_CHARS);
        let out = format!("noise\n{line}\nmore noise\n");
        assert_eq!(parse_box_report(&out), Some(r));
        let mut bad = BoxReport::new(Mode::Apply);
        for i in 0..100 {
            bad.problem(format!("p{i}"));
        }
        let (bad, _) = bad.finish();
        assert!(!bad.ok);
        assert_eq!(bad.problems.len(), REPORT_MAX_PROBLEMS);
        assert_eq!(bad.problems_total, 100);
        assert_eq!(parse_box_report("nothing here"), None);
    }

    #[test]
    fn test_mode_and_phase_parse_round_trip() {
        assert_eq!(Mode::parse("dry"), Some(Mode::Dry));
        assert_eq!(Mode::parse(Mode::Apply.as_str()), Some(Mode::Apply));
        assert_eq!(Mode::parse("x"), None);
        assert_eq!(
            Phase::parse_list("cloud"),
            Some(vec![Phase::S3, Phase::Snapshot])
        );
        for p in [Phase::Preflight, Phase::Box, Phase::S3, Phase::Snapshot] {
            assert_eq!(Phase::parse_list(p.as_str()), Some(vec![p]));
        }
        assert_eq!(Phase::parse_list("all"), None);
    }

    #[test]
    fn test_now_ist_is_utc_plus_five_thirty() {
        let t = Utc.with_ymd_and_hms(2026, 10, 31, 18, 30, 0).unwrap();
        assert_eq!(now_ist(t), at("2026-11-01 00:00"));
    }
}
