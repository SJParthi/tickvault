//! The 2026-10-22 production rebuild (plan item 66; operator Quotes 32, 33,
//! 34 in `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`):
//! a new 150 GB gp3 root (3,000 IOPS, 125 MiB/s) on the SAME instance,
//! retyped r8g.xlarge → r8g.large, after every QuestDB partition and every
//! on-box data file is copied to `s3://tv-prod-cold` and verified, with the
//! old 600 GB root snapshotted and KEPT, detached, untouched.
//!
//! This module is the pure half shared by the two programs that run the
//! rebuild:
//!
//! - `tv-rebuild-2026-10-22` (`src/bin/rebuild_2026_10_22.rs`), run by
//!   `.github/workflows/rebuild-2026-10-22.yml` on a GitHub runner: EC2, SSM
//!   and the deploy dispatch.
//! - `tickvault rebuild-box <phase>` (`rebuild_box.rs`), run ON the box over
//!   SSM by a copy of the deployed binary: the QuestDB, disk and S3 work.
//!
//! # The steps, in order (each verified before the next)
//!
//! | Phase | Dry run | Apply |
//! |---|---|---|
//! | `preflight` | everything below is read-only | same |
//! | `copy` | box inventory: tables, partitions, rows, bytes, WAL, files | app stopped; every partition, keep table, WAL segment and data file copied and verified; restore rehearsal |
//! | `snapshot` | names the volume | snapshot of the old root, tagged, waited |
//! | `swap` | prints the new volume spec | new 150 GB root attached, type r8g.large, started |
//! | `prepare-new` | — | QuestDB env for 2 vCPU / 16 GiB, keep tables + files restored before the app's first start |
//! | `deploy` | — | `deploy-aws` dispatched and waited |
//! | `postcheck` | — | app, disk, memory, tables checked; box stopped |
//! | `rollback` | names the volumes | old root re-attached, type r8g.xlarge |
//!
//! Any failure from `swap` onward rolls back automatically: the old root is
//! re-attached and the type restored, so the 08:30 IST start boots the box
//! exactly as it was.
//!
//! Cold path, one-off. Removed (with the workflow, the box subcommand and the
//! budget rows) by the first PR after the run.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use serde::{Deserialize, Serialize};

/// The cold bucket. Explicit, never derived from the environment.
pub const COLD_BUCKET: &str = "tv-prod-cold";
/// Prefix of every rebuild object in the cold bucket (the partition copies
/// excepted: they use the archiver's `questdb-partitions/` layout).
pub const S3_PREFIX: &str = tickvault_storage::rebuild_copy::REBUILD_S3_PREFIX;
/// `Name` tag of the production instance.
pub const INSTANCE_NAME_TAG: &str = "tv-prod-app";
/// The type the box runs today.
pub const OLD_INSTANCE_TYPE: &str = "r8g.xlarge";
/// The type the rebuild moves it to (Quote 32 step 4).
pub const NEW_INSTANCE_TYPE: &str = "r8g.large";
/// The root size the box runs today.
pub const OLD_ROOT_SIZE_GB: i32 = 600;
/// The new root (Quote 32 step 3).
pub const NEW_ROOT_SIZE_GB: i32 = 150;
/// The new root's IOPS (the free gp3 baseline, Quote 30).
pub const NEW_ROOT_IOPS: i32 = 3000;
/// The new root's throughput (the free gp3 baseline, Quote 33).
pub const NEW_ROOT_THROUGHPUT_MIBPS: i32 = 125;
/// Tag every object this rebuild creates carries.
pub const REBUILD_TAG_KEY: &str = "tv-rebuild";
/// Value of [`REBUILD_TAG_KEY`].
pub const REBUILD_TAG_VALUE: &str = "2026-10-22";
/// The old-root snapshot (and the old root volume) are kept until the
/// Quote 35 wipe on the night of 2026-10-31: more than the 7 days Quote 32
/// step 3 asks for. Nothing deletes by this tag; it records the date.
pub const SNAPSHOT_KEEP_UNTIL: &str = "2026-10-31";
/// Typed confirmation for any `apply` other than `rollback`.
pub const APPLY_CONFIRM: &str = "REBUILD-2026-10-22";
/// Typed confirmation for `rollback`.
pub const ROLLBACK_CONFIRM: &str = "ROLLBACK-2026-10-22";
/// The operator keep-alive the hourly curfew and the hard-stop guard honour.
pub const KEEP_ALIVE_PARAM: &str = "/tickvault/prod/keep-alive-until";
/// The app's drain hold (Quote 31).
pub const DRAIN_HOLD_PARAM: &str = "/tickvault/prod/drain-hold";
/// Hours the keep-alive is set ahead while the rebuild runs.
pub const KEEP_ALIVE_HOURS: i64 = 12;
/// The SSM agent's command timeout for one box phase (the copy can take
/// hours: ~262 GB of QuestDB on 2026-10-21).
pub const BOX_PHASE_TIMEOUT_SECS: u64 = 6 * 3600;
/// Prefix of the one JSON line a box phase prints for the runner.
pub const BOX_REPORT_PREFIX: &str = "REBUILD-REPORT ";
/// Most problem lines a report carries (SSM returns at most 24,000
/// characters of output; the full detail is in the box's local report file).
pub const REPORT_MAX_PROBLEMS: usize = 40;
/// Most fact lines a report carries.
pub const REPORT_MAX_LINES: usize = 80;
/// Workflow files whose queued or running runs block an apply.
pub const DEPLOY_WORKFLOWS: [&str; 3] = [
    "deploy-aws.yml",
    "deploy-aws-after-close.yml",
    "postmerge-catchup.yml",
];

const IST_OFFSET_SECS: i32 = 5 * 3600 + 30 * 60;

/// IST as a fixed offset (no DST).
#[must_use]
pub fn ist() -> FixedOffset {
    FixedOffset::east_opt(IST_OFFSET_SECS).unwrap_or_else(|| chrono::Offset::fix(&Utc))
}

/// What a run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Read and report only.
    Dry,
    /// Change things.
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

/// One step of the rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Read-only checks.
    Preflight,
    /// Copy everything on the old box to S3 and verify.
    Copy,
    /// Snapshot the old root.
    Snapshot,
    /// New root, new type.
    Swap,
    /// QuestDB env, restore keep tables and files on the new root.
    PrepareNew,
    /// Dispatch and wait for `deploy-aws`.
    Deploy,
    /// Verify the new box; stop it.
    Postcheck,
    /// Back to the old root and type.
    Rollback,
}

impl Phase {
    /// Parses the workflow input word.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "preflight" => Self::Preflight,
            "copy" => Self::Copy,
            "snapshot" => Self::Snapshot,
            "swap" => Self::Swap,
            "prepare-new" => Self::PrepareNew,
            "deploy" => Self::Deploy,
            "postcheck" => Self::Postcheck,
            "rollback" => Self::Rollback,
            _ => return None,
        })
    }

    /// Wire word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Copy => "copy",
            Self::Snapshot => "snapshot",
            Self::Swap => "swap",
            Self::PrepareNew => "prepare-new",
            Self::Deploy => "deploy",
            Self::Postcheck => "postcheck",
            Self::Rollback => "rollback",
        }
    }

    /// True for a phase that runs ON the box (the box subcommand's word).
    #[must_use]
    pub const fn runs_on_box(self) -> bool {
        matches!(self, Self::Copy | Self::PrepareNew | Self::Postcheck)
    }
}

/// The phases a workflow input runs, in order. `all` is every forward phase;
/// `rollback` is only ever itself.
#[must_use]
pub fn phases_for(input: &str) -> Option<Vec<Phase>> {
    if input == "all" {
        return Some(vec![
            Phase::Preflight,
            Phase::Copy,
            Phase::Snapshot,
            Phase::Swap,
            Phase::PrepareNew,
            Phase::Deploy,
            Phase::Postcheck,
        ]);
    }
    let one = Phase::parse(input)?;
    if one == Phase::Preflight || one == Phase::Rollback {
        Some(vec![one])
    } else {
        Some(vec![Phase::Preflight, one])
    }
}

/// The first night the rebuild may run: Thursday 2026-10-22, 17:30 IST
/// (Quote 33: "first allowed on the night of Thursday 2026-10-22").
#[must_use]
pub fn first_allowed(ist_tz: FixedOffset) -> Option<DateTime<FixedOffset>> {
    ist_tz
        .from_local_datetime(&NaiveDate::from_ymd_opt(2026, 10, 22)?.and_hms_opt(17, 30, 0)?)
        .single()
}

/// The last instant the tool acts: Saturday 2026-10-31, 08:00 IST — before
/// the Quote 35 wipe night. After it, a rebuild needs its own dated quote.
#[must_use]
pub fn last_allowed(ist_tz: FixedOffset) -> Option<DateTime<FixedOffset>> {
    ist_tz
        .from_local_datetime(&NaiveDate::from_ymd_opt(2026, 10, 31)?.and_hms_opt(8, 0, 0)?)
        .single()
}

/// `Ok` when an `apply` may run now: inside the 2026-10-22 .. 2026-10-31
/// fence AND on a weeknight between 17:30 and 08:00 IST (the night belongs
/// to the day it starts on, Monday to Friday). Quote 33 amends Quote 32's
/// "non-trading day" to exactly this window; the market (09:00–15:45 IST)
/// is therefore never inside it.
///
/// # Errors
/// The reason, in one line.
pub fn apply_window(now: DateTime<Utc>) -> Result<(), String> {
    let tz = ist();
    let local = now.with_timezone(&tz);
    let (Some(first), Some(last)) = (first_allowed(tz), last_allowed(tz)) else {
        return Err(String::from("window bounds could not be computed"));
    };
    if local < first {
        return Err(format!(
            "too early: {} IST is before the first allowed night ({} IST, Quote 33)",
            local.format("%Y-%m-%d %H:%M"),
            first.format("%Y-%m-%d %H:%M")
        ));
    }
    if local >= last {
        return Err(format!(
            "too late: {} IST is past {} IST; a later rebuild needs its own dated quote",
            local.format("%Y-%m-%d %H:%M"),
            last.format("%Y-%m-%d %H:%M")
        ));
    }
    let t = local.time();
    let evening = NaiveTime::from_hms_opt(17, 30, 0).unwrap_or(NaiveTime::MIN);
    let morning = NaiveTime::from_hms_opt(8, 0, 0).unwrap_or(NaiveTime::MIN);
    let weekday = local.weekday();
    let weeknight_evening = t >= evening
        && matches!(
            weekday,
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
        );
    let weeknight_morning = t < morning
        && matches!(
            weekday,
            Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri | Weekday::Sat
        );
    if weeknight_evening || weeknight_morning {
        Ok(())
    } else {
        Err(format!(
            "{} IST ({weekday}) is outside the 17:30–08:00 IST weeknight window",
            local.format("%Y-%m-%d %H:%M")
        ))
    }
}

/// The latest IST time of the night the disk swap may START: it needs about
/// three hours (swap, first-boot provisioning, restore, deploy, checks)
/// before the 08:00 IST end of the window and the 08:30 boot (owner, 2026-10-27:
/// "only swap if at least 3 hours remain").
pub const SWAP_LATEST_START_IST: (u32, u32) = (5, 0);

/// Minutes of one workflow job after which a multi-phase run does not start
/// the swap: the job is killed at 360 minutes (the cargo build takes part of
/// it) and a kill after the swap starts skips the rollback, so the swap,
/// prepare-new and deploy need the rest of the job.
pub const SWAP_LATEST_JOB_MINUTES: i64 = 180;

/// `Ok` when the disk swap may start now: inside the apply window and before
/// [`SWAP_LATEST_START_IST`] on a morning (a swap started in the evening has
/// the whole night). Pure, O(1).
///
/// # Errors
/// The reason, in one line.
pub fn swap_start_ok(now: DateTime<Utc>) -> Result<(), String> {
    apply_window(now)?;
    let local = now.with_timezone(&ist());
    let latest = NaiveTime::from_hms_opt(SWAP_LATEST_START_IST.0, SWAP_LATEST_START_IST.1, 0)
        .unwrap_or(NaiveTime::MIN);
    let morning_end = NaiveTime::from_hms_opt(8, 0, 0).unwrap_or(NaiveTime::MIN);
    let t = local.time();
    if t >= latest && t < morning_end {
        return Err(format!(
            "{} IST is too close to 08:00 to swap disks safely; the copy and snapshot stay, \
             run phase=swap the next night",
            local.format("%H:%M")
        ));
    }
    Ok(())
}

/// The keep-alive value: `now + KEEP_ALIVE_HOURS`, in IST with an explicit
/// offset (a shape both guard Lambdas accept).
#[must_use]
pub fn keep_alive_value(now: DateTime<Utc>) -> String {
    (now + chrono::TimeDelta::hours(KEEP_ALIVE_HOURS))
        .with_timezone(&ist())
        .format("%Y-%m-%dT%H:%M:%S+05:30")
        .to_string()
}

/// `true` when the drain-hold record is a fresh hold (`state = holding`,
/// `until_unix` in the future and at most an hour ahead) — the app reports
/// the day's data is not yet in the database. Any other record, or an
/// unreadable one, is no hold (the same reading the stop Lambdas use).
#[must_use]
pub fn drain_hold_active(raw: &str, now_unix: i64) -> bool {
    #[derive(Deserialize)]
    struct Record {
        v: i64,
        state: String,
        until_unix: i64,
    }
    serde_json::from_str::<Record>(raw).is_ok_and(|r| {
        r.v == 1
            && r.state == "holding"
            && r.until_unix > now_unix
            && r.until_unix <= now_unix.saturating_add(3600)
    })
}

/// What the runner reads about the instance and its root volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceFacts {
    /// Instance id.
    pub instance_id: String,
    /// `pending` / `running` / `stopping` / `stopped` / ...
    pub state: String,
    /// Instance type.
    pub instance_type: String,
    /// Availability zone.
    pub availability_zone: String,
    /// Root device name (e.g. `/dev/xvda`).
    pub root_device_name: String,
    /// The volume at the root device, if any.
    pub root_volume_id: Option<String>,
}

/// What the runner reads about one volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeFacts {
    /// Volume id.
    pub volume_id: String,
    /// GiB.
    pub size_gb: i32,
    /// Provisioned IOPS.
    pub iops: i32,
    /// MiB/s.
    pub throughput: i32,
    /// `gp3` expected.
    pub volume_type: String,
    /// `in-use` / `available` / ...
    pub state: String,
    /// Availability zone.
    pub availability_zone: String,
    /// Encrypted at rest.
    pub encrypted: bool,
    /// The value of the rebuild tag, if any.
    pub rebuild_tag: Option<String>,
}

/// Where the box stands relative to the rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// The old type with the old 600 GB root attached: nothing changed yet.
    Untouched,
    /// The new type with a rebuild-tagged 150 GB root attached.
    Swapped,
    /// Anything else, named. The tool refuses to act on it except `rollback`.
    Mixed(String),
}

/// Classifies the instance and its root.
#[must_use]
pub fn classify_stage(inst: &InstanceFacts, root: Option<&VolumeFacts>) -> Stage {
    let Some(root) = root else {
        return Stage::Mixed(String::from("no volume is attached at the root device"));
    };
    let old_root = root.size_gb == OLD_ROOT_SIZE_GB && root.rebuild_tag.is_none();
    let new_root =
        root.size_gb == NEW_ROOT_SIZE_GB && root.rebuild_tag.as_deref() == Some(REBUILD_TAG_VALUE);
    match (inst.instance_type.as_str(), old_root, new_root) {
        (OLD_INSTANCE_TYPE, true, _) => Stage::Untouched,
        (NEW_INSTANCE_TYPE, _, true) => Stage::Swapped,
        (t, _, _) => Stage::Mixed(format!(
            "type {t} with a {} GB root (rebuild tag {:?})",
            root.size_gb, root.rebuild_tag
        )),
    }
}

/// Problems that stop the forward phases, from the facts the runner read.
/// Empty means go.
#[must_use]
pub fn preflight_problems(
    inst: &InstanceFacts,
    root: Option<&VolumeFacts>,
    root_modifying: bool,
    drain_hold: bool,
    deploys_busy: &[String],
    holiday_today: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if inst.instance_id.is_empty() {
        out.push(format!("no instance tagged Name={INSTANCE_NAME_TAG}"));
    }
    if let Stage::Mixed(why) = classify_stage(inst, root) {
        out.push(format!("the box is in no known rebuild stage: {why}"));
    }
    if let Some(root) = root {
        if root.volume_type != "gp3" {
            out.push(format!("root volume type is {}, not gp3", root.volume_type));
        }
        if root.availability_zone != inst.availability_zone {
            out.push(format!(
                "root volume zone {} differs from the instance zone {}",
                root.availability_zone, inst.availability_zone
            ));
        }
    }
    if root_modifying {
        out.push(String::from(
            "a modification of the root volume is still in progress",
        ));
    }
    if drain_hold {
        out.push(String::from(
            "the drain hold is active: the app reports today's data is not yet in the database \
             (Quote 31); run again once it releases",
        ));
    }
    for d in deploys_busy {
        out.push(format!("deploy workflow busy: {d}"));
    }
    if holiday_today {
        out.push(String::from(
            "today (IST) is an NSE holiday: the box's holiday gate would stop it on boot",
        ));
    }
    out
}

/// The one JSON line a box phase prints for the runner (and saves in full
/// to its local report file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxReport {
    /// Phase word.
    pub phase: String,
    /// `dry` / `apply`.
    pub mode: String,
    /// True only when every check passed.
    pub ok: bool,
    /// Why not, one line each (capped at [`REPORT_MAX_PROBLEMS`]).
    pub problems: Vec<String>,
    /// How many problems there were in total.
    pub problems_total: usize,
    /// What it found or did, one line each (capped at [`REPORT_MAX_LINES`]).
    pub lines: Vec<String>,
    /// The copy manifest's key, when the copy wrote one.
    pub manifest_key: Option<String>,
}

impl BoxReport {
    /// A new report for `phase` / `mode`.
    #[must_use]
    pub fn new(phase: Phase, mode: Mode) -> Self {
        Self {
            phase: phase.as_str().to_string(),
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

/// Longest report line kept, in characters. 40 problems and 80 lines at this
/// width stay under the 24,000 characters SSM returns, and SSM truncates the
/// END of the output, where the report line is.
pub const REPORT_LINE_MAX_CHARS: usize = 160;

/// `line` cut to [`REPORT_LINE_MAX_CHARS`] characters (on a char boundary).
#[must_use]
pub fn clip(line: String) -> String {
    if line.chars().count() <= REPORT_LINE_MAX_CHARS {
        return line;
    }
    let mut out: String = line.chars().take(REPORT_LINE_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// The LAST report line in a box phase's output.
#[must_use]
pub fn parse_box_report(stdout: &str) -> Option<BoxReport> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(BOX_REPORT_PREFIX))
        .and_then(|json| serde_json::from_str(json).ok())
}

/// One keep-list table copied whole, with the DDL that recreates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepTableCopy {
    /// The `SHOW CREATE TABLE` text from the old box.
    pub ddl: String,
    /// The verified whole-table export.
    pub data: tickvault_storage::partition_archive::RebuildCopiedObject,
}

/// Everything the copy phase put in the cold bucket, and what the restore
/// reads back. Written once per copy run to a new key, never overwritten.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyManifest {
    /// Format version.
    pub version: u32,
    /// When the copy finished (UTC, RFC 3339).
    pub created_utc: String,
    /// Partition copies (market data and every other time-partitioned
    /// table), at the archiver's keys.
    pub partitions: Vec<tickvault_storage::partition_archive::RebuildCopiedObject>,
    /// Tables with no time partitions, copied whole.
    pub whole_tables: Vec<tickvault_storage::partition_archive::RebuildCopiedObject>,
    /// The keep-list, copied whole with DDL; restored onto the new disk.
    pub keep_tables: Vec<KeepTableCopy>,
    /// Files from `/opt/tickvault/data` (WAL, logs and temp excepted);
    /// restored onto the new disk.
    pub files: Vec<tickvault_storage::rebuild_copy::RebuildCopiedFile>,
    /// Raw WAL segments with a verified copy in `raw-frames/` at the end.
    pub wal_segments_verified: u64,
}

/// Manifest format version.
pub const MANIFEST_VERSION: u32 = 1;

/// The key a copy run's manifest is written to.
#[must_use]
pub fn manifest_key(created: DateTime<Utc>) -> String {
    format!(
        "{S3_PREFIX}/manifests/copy-{}.json",
        created.format("%Y%m%dT%H%M%SZ")
    )
}

/// True for a key [`manifest_key`] could have produced (the runner passes the
/// key the copy reported to the restore; anything else is refused).
#[must_use]
pub fn is_manifest_key(key: &str) -> bool {
    let Some(stamp) = key
        .strip_prefix(&format!("{S3_PREFIX}/manifests/copy-"))
        .and_then(|r| r.strip_suffix(".json"))
    else {
        return false;
    };
    stamp.len() == 16
        && stamp.as_bytes()[8] == b'T'
        && stamp.ends_with('Z')
        && stamp
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || i == 15 || b.is_ascii_digit())
}

/// True for a 40-hex-digit git commit id (the deploy artifact the box runs).
#[must_use]
pub fn is_commit_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Tables the new disk must NOT hold once ITEM 62 is deployed (Quote 34).
/// The postcheck reports them; their presence means the deployed build still
/// predates ITEM 62, not that the rebuild created them (it restores only the
/// keep-list).
pub const QUOTE_34_REMOVED_TABLES: [&str; 6] = [
    "candles_3s",
    "candles_5s",
    "top_volume_1s",
    "top_volume_3s",
    "top_volume_5s",
    "top_volume_1m",
];

/// `true` for a table Quote 34 removes: the six named ones, and any other
/// `top_volume*` table ("the entire top volume tables", which also covers the
/// legacy `top_volume` / `top_volume_rank` names still on the keep list).
/// Such a table is copied to S3 like any other and never re-created on the
/// new disk.
#[must_use]
pub fn is_quote_34_removed(table: &str) -> bool {
    QUOTE_34_REMOVED_TABLES.contains(&table) || table.starts_with("top_volume")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_ist(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        ist()
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(y, m, d)
                    .and_then(|x| x.and_hms_opt(h, min, 0))
                    .expect("valid"),
            )
            .single()
            .expect("unique")
            .with_timezone(&Utc)
    }

    #[test]
    fn test_apply_window_opens_thursday_1730_ist_and_only_on_weeknights() {
        assert!(apply_window(at_ist(2026, 10, 22, 17, 29)).is_err());
        assert!(apply_window(at_ist(2026, 10, 22, 17, 30)).is_ok());
        assert!(apply_window(at_ist(2026, 10, 22, 23, 59)).is_ok());
        // Friday 07:59 belongs to Thursday night; 08:00 closes it.
        assert!(apply_window(at_ist(2026, 10, 23, 7, 59)).is_ok());
        assert!(apply_window(at_ist(2026, 10, 23, 8, 0)).is_err());
        // Market hours are never inside.
        assert!(apply_window(at_ist(2026, 10, 23, 10, 0)).is_err());
        // Friday night is a weeknight; Saturday morning belongs to it.
        assert!(apply_window(at_ist(2026, 10, 23, 18, 0)).is_ok());
        assert!(apply_window(at_ist(2026, 10, 24, 7, 0)).is_ok());
        // Saturday evening and Sunday are not.
        assert!(apply_window(at_ist(2026, 10, 24, 18, 0)).is_err());
        assert!(apply_window(at_ist(2026, 10, 25, 3, 0)).is_err());
        // Monday morning before 08:00 belongs to Sunday night: refused.
        assert!(apply_window(at_ist(2026, 10, 26, 7, 0)).is_err());
        // The fence ends Saturday 31 Oct 08:00 IST.
        assert!(apply_window(at_ist(2026, 10, 30, 18, 0)).is_ok());
        assert!(apply_window(at_ist(2026, 10, 31, 8, 0)).is_err());
    }

    #[test]
    fn test_phases_for_wraps_every_forward_phase_in_a_preflight() {
        assert_eq!(phases_for("all").map(|v| v.len()), Some(7));
        assert_eq!(
            phases_for("copy"),
            Some(vec![Phase::Preflight, Phase::Copy])
        );
        assert_eq!(phases_for("rollback"), Some(vec![Phase::Rollback]));
        assert_eq!(phases_for("preflight"), Some(vec![Phase::Preflight]));
        assert_eq!(phases_for("nuke"), None);
        for p in [
            Phase::Preflight,
            Phase::Copy,
            Phase::Snapshot,
            Phase::Swap,
            Phase::PrepareNew,
            Phase::Deploy,
            Phase::Postcheck,
            Phase::Rollback,
        ] {
            assert_eq!(Phase::parse(p.as_str()), Some(p));
        }
        assert!(Phase::Copy.runs_on_box() && !Phase::Swap.runs_on_box());
        assert_eq!(Mode::parse("apply"), Some(Mode::Apply));
        assert_eq!(Mode::parse("APPLY"), None);
    }

    #[test]
    fn test_swap_start_ok_refuses_from_0500_ist_until_the_window_closes() {
        assert!(swap_start_ok(at_ist(2026, 10, 27, 18, 0)).is_ok());
        assert!(swap_start_ok(at_ist(2026, 10, 27, 23, 59)).is_ok());
        assert!(swap_start_ok(at_ist(2026, 10, 28, 4, 59)).is_ok());
        assert!(swap_start_ok(at_ist(2026, 10, 28, 5, 0)).is_err());
        assert!(swap_start_ok(at_ist(2026, 10, 28, 7, 59)).is_err());
        // Outside the apply window altogether.
        assert!(swap_start_ok(at_ist(2026, 10, 28, 12, 0)).is_err());
    }

    /// The ec2:CreateTags permission probe sends this tag; EC2 refuses an
    /// empty key or value as InvalidParameterValue before it checks the
    /// permission, which made the probe report a missing permission
    /// (2026-10-27).
    #[test]
    fn test_regression_permission_probe_tag_is_not_empty() {
        assert!(!REBUILD_TAG_KEY.trim().is_empty());
        assert!(!REBUILD_TAG_VALUE.trim().is_empty());
    }

    #[test]
    fn test_keep_alive_value_is_twelve_hours_ahead_in_ist() {
        let now = at_ist(2026, 10, 22, 18, 0);
        assert_eq!(keep_alive_value(now), "2026-10-23T06:00:00+05:30");
    }

    #[test]
    fn test_drain_hold_active_only_for_a_fresh_holding_record() {
        let now = 1_000_000;
        let rec = |state: &str, until: i64| {
            format!(
                r#"{{"v":1,"state":"{state}","until_unix":{until},"written_unix":{now},"left":"x"}}"#
            )
        };
        assert!(drain_hold_active(&rec("holding", now + 600), now));
        assert!(!drain_hold_active(&rec("released", now + 600), now));
        assert!(!drain_hold_active(&rec("holding", now - 1), now));
        assert!(!drain_hold_active(&rec("holding", now + 3601), now));
        assert!(!drain_hold_active("", now));
        assert!(!drain_hold_active("garbage", now));
    }

    fn inst(t: &str) -> InstanceFacts {
        InstanceFacts {
            instance_id: "i-0c3fe906dad5492fc".into(),
            state: "stopped".into(),
            instance_type: t.into(),
            availability_zone: "ap-south-1b".into(),
            root_device_name: "/dev/xvda".into(),
            root_volume_id: Some("vol-1".into()),
        }
    }

    fn vol(size: i32, tag: Option<&str>) -> VolumeFacts {
        VolumeFacts {
            volume_id: "vol-1".into(),
            size_gb: size,
            iops: 3000,
            throughput: 125,
            volume_type: "gp3".into(),
            state: "in-use".into(),
            availability_zone: "ap-south-1b".into(),
            encrypted: true,
            rebuild_tag: tag.map(str::to_string),
        }
    }

    #[test]
    fn test_classify_stage_names_the_three_states() {
        assert_eq!(
            classify_stage(&inst("r8g.xlarge"), Some(&vol(600, None))),
            Stage::Untouched
        );
        assert_eq!(
            classify_stage(&inst("r8g.large"), Some(&vol(150, Some("2026-10-22")))),
            Stage::Swapped
        );
        assert!(matches!(
            classify_stage(&inst("r8g.large"), Some(&vol(600, None))),
            Stage::Mixed(_)
        ));
        assert!(matches!(
            classify_stage(&inst("r8g.xlarge"), Some(&vol(150, Some("2026-10-22")))),
            Stage::Mixed(_)
        ));
        assert!(matches!(
            classify_stage(&inst("r8g.xlarge"), None),
            Stage::Mixed(_)
        ));
    }

    #[test]
    fn test_preflight_problems_lists_every_blocker() {
        let ok = preflight_problems(
            &inst("r8g.xlarge"),
            Some(&vol(600, None)),
            false,
            false,
            &[],
            false,
        );
        assert!(ok.is_empty(), "{ok:?}");
        let busy = vec![String::from("deploy-aws.yml in_progress")];
        let mut other_zone = vol(600, None);
        other_zone.availability_zone = "ap-south-1a".into();
        other_zone.volume_type = "gp2".into();
        let bad = preflight_problems(
            &inst("r8g.xlarge"),
            Some(&other_zone),
            true,
            true,
            &busy,
            true,
        );
        assert_eq!(bad.len(), 6, "{bad:?}");
    }

    #[test]
    fn test_box_report_round_trips_and_caps_its_lists() {
        let mut r = BoxReport::new(Phase::Copy, Mode::Apply);
        for i in 0..(REPORT_MAX_PROBLEMS + 5) {
            r.problem(format!("p{i}"));
        }
        for i in 0..(REPORT_MAX_LINES + 5) {
            r.line(format!("l{i}"));
        }
        let (r, out) = r.finish();
        assert!(!r.ok);
        assert_eq!(r.problems.len(), REPORT_MAX_PROBLEMS);
        assert_eq!(r.problems_total, REPORT_MAX_PROBLEMS + 5);
        assert_eq!(r.lines.len(), REPORT_MAX_LINES);
        let text = format!("noise\n{out}\n");
        assert_eq!(parse_box_report(&text), Some(r));
        assert_eq!(parse_box_report("no report here"), None);
        let (clean, _) = BoxReport::new(Phase::Postcheck, Mode::Dry).finish();
        assert!(clean.ok);
    }

    #[test]
    fn test_manifest_key_round_trips_through_is_manifest_key() {
        let created = at_ist(2026, 10, 22, 19, 5);
        let key = manifest_key(created);
        assert_eq!(
            key,
            "rebuild-2026-10-22/manifests/copy-20261022T133500Z.json"
        );
        assert!(is_manifest_key(&key));
        for bad in [
            "rebuild-2026-10-22/manifests/copy-x.json",
            "rebuild-2026-10-22/manifests/copy-20261022T133500Z.json; rm -rf /",
            "questdb-partitions/x.json",
            "",
        ] {
            assert!(!is_manifest_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn test_first_allowed_and_last_allowed_bound_the_run() {
        let tz = ist();
        let first = first_allowed(tz).expect("valid date");
        let last = last_allowed(tz).expect("valid date");
        assert_eq!(first.to_rfc3339(), "2026-10-22T17:30:00+05:30");
        assert_eq!(last.to_rfc3339(), "2026-10-31T08:00:00+05:30");
        assert!(first < last);
    }

    #[test]
    fn test_parse_box_report_reads_the_last_report_line_only() {
        let (a, line_a) = BoxReport::new(Phase::Copy, Mode::Dry).finish();
        let mut b = BoxReport::new(Phase::Copy, Mode::Apply);
        b.problem("disk");
        let (b, line_b) = b.finish();
        let text = format!("{line_a}\nsome output\n{line_b}\n");
        assert_eq!(parse_box_report(&text), Some(b));
        assert_ne!(parse_box_report(&text), Some(a));
        assert_eq!(
            parse_box_report(&format!("{BOX_REPORT_PREFIX}not json")),
            None
        );
    }

    #[test]
    fn test_is_quote_34_removed_covers_the_six_and_every_top_volume_name() {
        for t in QUOTE_34_REMOVED_TABLES {
            assert!(is_quote_34_removed(t), "{t}");
        }
        assert!(is_quote_34_removed("top_volume_rank"));
        assert!(is_quote_34_removed("top_volume"));
        for keep in ["candles_1s", "candles_1m", "instrument_lifecycle", "ticks"] {
            assert!(!is_quote_34_removed(keep), "{keep}");
        }
    }

    #[test]
    fn test_clip_cuts_long_lines_on_a_char_boundary() {
        let short = String::from("ok");
        assert_eq!(clip(short.clone()), short);
        let long: String = "é".repeat(REPORT_LINE_MAX_CHARS + 10);
        let cut = clip(long);
        assert_eq!(cut.chars().count(), REPORT_LINE_MAX_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn test_is_commit_sha_accepts_only_a_lowercase_40_hex_id() {
        assert!(is_commit_sha("f92a6472f0000000000000000000000000000000"));
        assert!(!is_commit_sha("F92A6472F0000000000000000000000000000000"));
        assert!(!is_commit_sha("f92a6472f"));
        assert!(!is_commit_sha("f92a6472f000000000000000000000000000000;"));
    }
}
