//! `tickvault day-repair <dry|apply|verify> <sha>`: the one-off repair of the
//! held day 2026-11-20 (daily-universe Quote 63 and its "Quote 63 tool"
//! table, plan ITEM UTC-REPAIR; the server step and the dispatch-only
//! workflow are rust-only lock §0.21).
//!
//! Dhan sent every Last Trade Time on 20 Nov as UTC, so the day's `ticks`,
//! `tick_quotes`, `feed_aux_packets` and `candles_*` rows carry the wrong
//! times. The repair, in order, each step noted in a progress file
//! ([`state`]) and a `day_repair_audit` row:
//!
//! 1. **Enumerate** the day's held-kind partitions and its seal, spill and
//!    DLQ files; read the guards (a 20 Nov WAL segment still waiting for
//!    replay, the offset fingerprint on 19 and 21 Nov, a Brutex `DONE`).
//! 2. **Copy** every partition and file, create-only, to
//!    `s3://tv-prod-cold/quarantine/2026-11-20-ltt-utc-shift/` and verify it.
//! 3. **Move** the files into `data/held/2026-11-20/` (rename only), before
//!    any drop, so a stopped run never leaves them for a boot to replay.
//! 4. **Drop** the partitions in QuestDB, each only after its verified copy
//!    (the drop re-exports and re-verifies right before it).
//! 5. **Rebuild** the day from its raw frames ([`rebuild`]).
//! 6. **Verify** (a separate invocation) and only then
//! 7. write `data/state/day-repair/2026-11-20.done` and release the app unit.
//!    A Brutex `DONE` for the day refuses `apply` (Quote 63 tool table: a
//!    sealed day's manifest is create-only, a superseding marker needs its
//!    own owner decision) and is checked again before `.done`
//!    ([`done_holds_release`]).
//!
//! The window is re-checked before the moves, before each drop, before the
//! rebuild and between rebuild segments; a run that reaches 08:00 IST on a
//! session day stops ([`WINDOW_STOP`]), releases the unit hold so the app
//! starts on time, and resumes in the next window.
//!
//! A re-run after the rebuild began finds the rebuilt (correct) rows in the
//! day's partitions and copies them to the quarantine prefix too (a `.sha-`
//! key beside the wrong copy) before dropping and rebuilding again: more
//! work and a larger quarantine, never a drop without a verified copy.
//!
//! Every resumed step re-checks its result (S3, QuestDB, the disk), never the
//! progress file alone. Never deletes anything in S3; never touches
//! `market_depth*`, cross-verify, SEBI or audit rows; never writes a canonical
//! key, a manifest, an amend record or a `partition_archive_audit` row (the
//! storage half, `partition_archive::quarantine_copy`, holds those rules).
//!
//! Refused 08:00 to 16:30 IST on a session day, after 2026-11-30 IST, while
//! the app is running, and while another process holds the WAL folder. The
//! runner refuses while a deploy is queued or running.
//!
//! O(partitions + files) for the bookkeeping, O(rows) for the copies and
//! O(frames + rows) for the rebuild; offline, once. Deleted with the
//! workflow and the SSM literal by the first PR after a passed verify.

pub mod rebuild;
pub mod state;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use chrono::{NaiveDate, NaiveDateTime, Timelike as _};
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_storage::held_days::{DAY_REPAIR_DIR, MarkerKind, MarkerRead};
use tracing::{error, info, warn};

use rebuild::RebuildReport;
use state::{ProgressLine, ProgressLog, Step, detail_u64, detail_value};

/// The first argument that selects this subcommand.
pub const DAY_REPAIR_SUBCOMMAND: &str = "day-repair";
/// The one day this tool repairs (Quote 63). Hard-coded on purpose: no other
/// date can be passed in.
pub const REPAIR_DAY: &str = "2026-11-20";
/// The app folder on the box (`data/` lives under it).
pub const APP_DIR: &str = "/opt/tickvault";
/// The app's systemd unit.
pub const APP_UNIT: &str = "tickvault";
/// A runtime drop-in that keeps the app unit from starting while the repair
/// runs (`ConditionPathExists=` a path that never exists). `systemctl mask`
/// is refused for a unit file under `/etc/systemd/system`, which is where
/// the box installs `tickvault.service`; `/run` is cleared by a reboot, so a
/// failed run blocks the app only until the box restarts (the day stays
/// held either way).
pub const UNIT_HOLD_DROPIN: &str =
    "/run/systemd/system/tickvault.service.d/99-day-repair-hold.conf";
/// Where the moved seal, spill and DLQ files go.
pub const HELD_FILES_DIR: &str = "data/held/2026-11-20";
/// Where segments are downloaded for the rebuild (created and emptied by
/// the run).
pub const STAGING_DIR: &str = "data/day-repair-staging/2026-11-20";
/// The last instant (IST) the tool runs; refused after it.
pub const REFUSED_AFTER_IST: &str = "2026-11-30T23:59:59";
/// The text an error carries when the run stopped because its window
/// closed (08:00 IST on a session day, or the tool's expiry). `run_mode`
/// then releases the unit hold so the app starts at its usual time; the day
/// stays held and a later run resumes.
pub const WINDOW_STOP: &str = "day repair window closed";
/// The progress object that stands for the whole day.
pub const DAY_OBJECT: &str = "day";

/// How long the keep-alive SSM write may take.
const KEEP_ALIVE_PUT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15); // APPROVED: this IS the named constant the rule asks for

/// Exit codes.
pub const EXIT_OK: i32 = 0;
/// A step failed (the day stays held; re-run resumes).
pub const EXIT_FAILED: i32 = 1;
/// Bad arguments.
pub const EXIT_USAGE: i32 = 2;
/// Refused by a guard (window, app running, WAL owned, ...).
pub const EXIT_REFUSED: i32 = 3;

/// The three invocations the runner chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Read only: guards and the object list.
    Dry,
    /// Steps 1 to 5.
    Apply,
    /// Steps 6 and 7.
    Verify,
}

impl Mode {
    /// The argument spelling.
    #[must_use]
    // TEST-EXEMPT: a three-arm label used in the run output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dry => "dry",
            Self::Apply => "apply",
            Self::Verify => "verify",
        }
    }
}

/// True when the process was started as `tickvault day-repair ...`.
#[must_use]
pub fn is_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(DAY_REPAIR_SUBCOMMAND)
}

/// Parses `tickvault day-repair <dry|apply|verify> <40-hex sha>`. Pure.
///
/// # Errors
/// A missing or unknown mode, a sha that is not 40 lowercase hex digits, or
/// extra arguments.
pub fn parse_args(args: &[String]) -> Result<(Mode, String), String> {
    let mode = match args.get(2).map(String::as_str) {
        Some("dry") => Mode::Dry,
        Some("apply") => Mode::Apply,
        Some("verify") => Mode::Verify,
        other => return Err(format!("mode must be dry, apply or verify, got {other:?}")),
    };
    let sha = args.get(3).cloned().unwrap_or_default();
    if sha.len() != 40
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("sha must be 40 lowercase hex digits, got {sha:?}"));
    }
    if args.len() > 4 {
        return Err("too many arguments".to_owned());
    }
    Ok((mode, sha))
}

/// The repaired day as a date.
///
/// # Errors
/// Never in practice (the constant parses).
// TEST-EXEMPT: parses the REPAIR_DAY constant; every driver test runs on the same date.
pub fn repair_day() -> anyhow::Result<NaiveDate> {
    NaiveDate::parse_from_str(REPAIR_DAY, "%Y-%m-%d").context("REPAIR_DAY")
}

/// Why the tool may not run at `now_ist` (IST wall time), or `None`.
/// `session_day` says whether `now_ist`'s date is a trading or Muhurat day.
/// Pure.
#[must_use]
// TEST-EXEMPT: covered by test_window_refuses_0800_to_1630_on_a_trading_day_and_after_2026_11_30
pub fn window_refusal(now_ist: NaiveDateTime, session_day: bool) -> Option<String> {
    let last = NaiveDateTime::parse_from_str(REFUSED_AFTER_IST, "%Y-%m-%dT%H:%M:%S").ok()?;
    if now_ist > last {
        return Some(format!(
            "refused after {REFUSED_AFTER_IST} IST (one-off tool expired)"
        ));
    }
    let secs = i64::from(now_ist.time().num_seconds_from_midnight());
    if session_day && (8 * 3_600..16 * 3_600 + 30 * 60).contains(&secs) {
        return Some("refused 08:00 to 16:30 IST on a trading day".to_owned());
    }
    None
}

/// `Ok` while the run window is open now, else an error carrying
/// [`WINDOW_STOP`]. Reads the wall clock and the configured calendar. Cold.
///
/// # Errors
/// The window is closed, or the calendar could not be built.
// TEST-EXEMPT: one wall-clock read around window_refusal, which is unit-tested.
pub fn window_open_now(config: &tickvault_common::config::ApplicationConfig) -> anyhow::Result<()> {
    let calendar =
        tickvault_common::trading_calendar::TradingCalendar::from_config(&config.trading)?;
    let now = now_ist();
    match window_refusal(now, calendar.is_session_day(now.date())) {
        None => Ok(()),
        Some(why) => bail!("{WINDOW_STOP}: {why}"),
    }
}

/// IST wall time now. O(1).
#[must_use]
// TEST-EXEMPT: one wall-clock read; window_refusal, which takes the reading, is unit-tested.
pub fn now_ist() -> NaiveDateTime {
    (chrono::Utc::now() + chrono::Duration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).naive_utc()
}

/// The UTC unix hours `[first, last]` whose span meets the IST `day`: a tick
/// spill file is named by the unix hour it was written in. O(1).
///
/// # Errors
/// As [`rebuild::day_receipt_bounds_nanos`].
// TEST-EXEMPT: covered by test_tick_spill_hour_and_day_file_classification
pub fn day_unix_hours(day: NaiveDate) -> anyhow::Result<(u64, u64)> {
    let (start, end) = rebuild::day_receipt_bounds_nanos(day)?;
    let first = u64::try_from(start / 1_000_000_000 / 3_600).context("hour")?;
    let last = u64::try_from((end / 1_000_000_000 - 1) / 3_600).context("hour")?;
    Ok((first, last))
}

/// The unix hour in a tick spill file name (`<prefix>-<feed>-<hour>.ilp`,
/// anything after `.ilp` allowed). Pure.
#[must_use]
pub fn tick_spill_hour(name: &str) -> Option<u64> {
    let stem = &name[..name.find(".ilp")?];
    let (_, hour) = stem.rsplit_once('-')?;
    hour.parse().ok()
}

/// Whether `rel` (relative to the app folder) is a seal, spill or DLQ file
/// of `day` that a later boot would replay over the rebuilt rows. Pure.
///
/// - `data/spill/seals_v4-<day>.bin*`, also under `data/spill/replaying/`;
/// - `data/dlq/seals_v4-<day>.ndjson*`, also under `data/dlq/replaying/`;
/// - `data/spill/ticks/<prefix>-<feed>-<hour>.ilp*` for an hour of the day.
///
/// Never `data/spill/audit` (SEBI/audit tier), `data/spill/depth` (depth is
/// not repaired), or a `refused/`, `archive/` or `quarantine/` folder (kept,
/// never replayed).
#[must_use]
// TEST-EXEMPT: covered by test_tick_spill_hour_and_day_file_classification
pub fn is_day_file(rel: &Path, day: &str, hours: (u64, u64)) -> bool {
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    let seal_prefix = format!("seals_v4-{day}.");
    if parent == Path::new("data/spill") || parent == Path::new("data/spill/replaying") {
        return name.starts_with(&seal_prefix) && name.contains(".bin");
    }
    if parent == Path::new("data/dlq") || parent == Path::new("data/dlq/replaying") {
        return name.starts_with(&seal_prefix) && name.contains(".ndjson");
    }
    if parent == Path::new("data/spill/ticks") {
        return tick_spill_hour(name).is_some_and(|h| h >= hours.0 && h <= hours.1);
    }
    false
}

/// Every day file under `root` in its source folder, sorted. O(files).
#[must_use]
// TEST-EXEMPT: covered by proptest_crash_and_resume_over_every_step and test_clean_run_copies_drops_moves_rebuilds_verifies_and_marks_done.
pub fn list_day_files(root: &Path, day: &str, hours: (u64, u64)) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in [
        "data/spill",
        "data/spill/replaying",
        "data/dlq",
        "data/dlq/replaying",
        "data/spill/ticks",
    ] {
        let Ok(rd) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in rd.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let rel = Path::new(dir).join(entry.file_name());
            if is_day_file(&rel, day, hours) {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

/// Where a moved day file lands: `data/<rest>` becomes
/// `data/held/<day>/<rest>`. Pure.
#[must_use]
// TEST-EXEMPT: covered by test_tick_spill_hour_and_day_file_classification and test_move_day_file_never_overwrites
pub fn held_target(rel: &Path) -> PathBuf {
    let rest = rel.strip_prefix("data").unwrap_or(rel);
    Path::new(HELD_FILES_DIR).join(rest)
}

/// WAL segments of `day` still waiting for replay (in the WAL folder or its
/// `replaying/`): replaying them next to the rebuilt rows would write the
/// wrong times again, so the repair refuses. O(files).
#[must_use]
pub fn waiting_wal_segments(wal_dir: &Path, day: NaiveDate) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in [wal_dir.to_path_buf(), wal_dir.join("replaying")] {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(nanos) = tickvault_storage::ws_frame_spill::segment_name_nanos(name) else {
                continue;
            };
            let Ok(nanos) = i64::try_from(nanos) else {
                continue;
            };
            if let Ok(bounds) = rebuild::day_receipt_bounds_nanos(day)
                && rebuild::receipt_day(nanos, bounds) == rebuild::ReceiptDay::On
            {
                out.push(dir.join(name));
            }
        }
    }
    out.sort();
    out
}

/// One held-kind partition of the day.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Partition {
    /// `ticks`, `tick_quotes`, `feed_aux_packets` or `candles_*`.
    pub table: String,
    /// `2026-11-20` (day partition) or `2026-11-20THH` (hour partition).
    pub partition: String,
}

impl Partition {
    /// The progress object: `table/partition`.
    #[must_use]
    pub fn object(&self) -> String {
        format!("{}/{}", self.table, self.partition)
    }

    /// Whether this partition is one Quote 63 lets the tool drop: a held kind
    /// (never `market_depth*`, cross-verify, SEBI or audit) of the repaired
    /// day. Pure.
    #[must_use]
    pub fn allowed(&self, day: &str) -> bool {
        tickvault_storage::held_days::table_is_held_kind(&self.table)
            && (self.partition == day
                || self
                    .partition
                    .strip_prefix(day)
                    .is_some_and(|rest| rest.starts_with('T')))
    }
}

/// One verified quarantine copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRecord {
    /// The S3 key the copy sits at.
    pub key: String,
    /// The SHA-256 of the copied bytes, lowercase hex.
    pub sha256: String,
    /// Rows the copy holds (0 for a file).
    pub rows: u64,
    /// Bytes the copy holds.
    pub bytes: u64,
}

impl CopyRecord {
    /// The progress detail.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "key={};sha256={};rows={};bytes={}",
            self.key, self.sha256, self.rows, self.bytes
        )
    }

    /// Parses [`Self::encode`].
    #[must_use]
    pub fn decode(detail: &str) -> Option<Self> {
        Some(Self {
            key: detail_value(detail, "key")?.to_owned(),
            sha256: detail_value(detail, "sha256")?.to_owned(),
            rows: detail_u64(detail, "rows")?,
            bytes: detail_u64(detail, "bytes")?,
        })
    }
}

/// What the read-only guards found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preflight {
    /// The day's held-kind partitions still in QuestDB.
    pub partitions: Vec<Partition>,
    /// Objects already at canonical `questdb-partitions/*/2026-11-20*` keys
    /// (kept; the export puts the corrected copy at `.sha-` and the manifest
    /// names it).
    pub canonical_objects: Vec<String>,
    /// Whether `questdb-partitions/_daily/2026-11-20/DONE` exists.
    pub brutex_done: bool,
    /// Index ticks stamped before 09:00 on 19, 20 and 21 Nov.
    pub fingerprints: [u64; 3],
}

/// Why `apply` must not start (empty means go). Pure.
#[must_use]
pub fn apply_refusals(pre: &Preflight, waiting_wal: &[PathBuf], day: &str) -> Vec<String> {
    let mut out = Vec::new();
    // The Quote 63 tool table (Brutex row and REJECT list): applying while
    // the day's DONE exists is refused; a superseding marker needs its own
    // owner decision. The verify re-checks it before `.done`
    // ([`done_holds_release`]).
    if pre.brutex_done {
        out.push(format!(
            "questdb-partitions/_daily/{day}/DONE already exists: DONE is create-only (Quote 60), \
             a superseding marker needs its own owner decision"
        ));
    }
    if pre.fingerprints[0] > 0 {
        out.push(format!(
            "the day before holds {} index ticks before 09:00 (the offset is not 20 Nov only)",
            pre.fingerprints[0]
        ));
    }
    if pre.fingerprints[2] > 0 {
        out.push(format!(
            "the day after holds {} index ticks before 09:00 (needs its own quote)",
            pre.fingerprints[2]
        ));
    }
    if !waiting_wal.is_empty() {
        out.push(format!(
            "{} WAL segment(s) of {day} still wait for replay: {}",
            waiting_wal.len(),
            waiting_wal
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for p in &pre.partitions {
        if !p.allowed(day) {
            out.push(format!(
                "the listing returned a partition outside Quote 63: {}",
                p.object()
            ));
        }
    }
    out
}

/// Why `.done` must not be written because the day's Brutex `DONE` exists
/// (it cannot appear while the day is held, so this is a second check of
/// the apply refusal), or `None`. A `DONE` is final and its manifest is
/// create-only (Quote 60), so after a release no export would ever name the
/// corrected `.sha-` copy (owner answer 2026-11-21: the manifest names it
/// BEFORE the release). Pure.
#[must_use]
// TEST-EXEMPT: covered by test_apply_refusals_name_each_guard_and_brutex_done_present_refuses_apply
pub fn done_holds_release(brutex_done: bool, day: &str) -> Option<String> {
    brutex_done.then(|| {
        format!(
            "questdb-partitions/_daily/{day}/DONE already exists, so no new manifest can name the \
             corrected copy (DONE and manifests are create-only, Quote 60); the database is \
             repaired but the day stays held until the owner decides how a sealed day is \
             superseded"
        )
    })
}

/// The storage, S3 and rebuild half the step driver calls; production uses
/// [`prod_io::ProdIo`], the tests a fake with crash injection.
pub(crate) trait RepairIo {
    /// The read-only guards and the partition list.
    async fn preflight(&mut self) -> anyhow::Result<Preflight>;
    /// The day's held-kind partitions still in QuestDB.
    async fn list_partitions(&mut self) -> anyhow::Result<Vec<Partition>>;
    /// Exports, places (create-only), recounts and verifies one partition.
    async fn copy_partition(&mut self, p: &Partition) -> anyhow::Result<CopyRecord>;
    /// Whether a copy recorded earlier is still in S3 with its size and SHA.
    async fn copy_still_verified(&mut self, rec: &CopyRecord) -> anyhow::Result<bool>;
    /// Drops one partition in QuestDB (after its verified copy).
    async fn drop_partition(&mut self, p: &Partition, rec: &CopyRecord) -> anyhow::Result<()>;
    /// Copies one local day file (create-only) and verifies it.
    async fn copy_file(&mut self, rel: &Path) -> anyhow::Result<CopyRecord>;
    /// Rebuilds the day from its raw frames.
    async fn rebuild(&mut self) -> anyhow::Result<RebuildReport>;
    /// The verify gate; returns the findings (empty = repaired).
    async fn verify(&mut self, rep: &RebuildReport) -> anyhow::Result<Vec<String>>;
    /// `Ok` while the run window is open; an error carrying [`WINDOW_STOP`]
    /// once 08:00 IST on a session day (or the tool's expiry) is reached.
    async fn window_open(&mut self) -> anyhow::Result<()>;
    /// One `day_repair_audit` row.
    async fn audit(
        &mut self,
        step: Step,
        object: &str,
        rec: Option<&CopyRecord>,
        outcome: &str,
        detail: &str,
    ) -> anyhow::Result<()>;
}

fn note(log: &mut ProgressLog, step: Step, object: &str, detail: String) -> anyhow::Result<()> {
    log.append(ProgressLine {
        step,
        object: object.to_owned(),
        detail,
    })
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Copies one partition unless a copy recorded earlier still verifies.
async fn ensure_copied<I: RepairIo>(
    io: &mut I,
    log: &mut ProgressLog,
    p: &Partition,
) -> anyhow::Result<CopyRecord> {
    let obj = p.object();
    if let Some(rec) = log
        .find(Step::Copy, &obj)
        .and_then(|l| CopyRecord::decode(&l.detail))
        && io.copy_still_verified(&rec).await?
    {
        return Ok(rec);
    }
    let rec = io.copy_partition(p).await?;
    note(log, Step::Copy, &obj, rec.encode())?;
    io.audit(Step::Copy, &obj, Some(&rec), "copied", "").await?;
    Ok(rec)
}

/// Copies one day file unless a copy recorded earlier still verifies.
async fn ensure_file_copied<I: RepairIo>(
    io: &mut I,
    log: &mut ProgressLog,
    rel: &Path,
) -> anyhow::Result<CopyRecord> {
    let obj = rel.display().to_string();
    if let Some(rec) = log
        .find(Step::Copy, &obj)
        .and_then(|l| CopyRecord::decode(&l.detail))
        && io.copy_still_verified(&rec).await?
    {
        return Ok(rec);
    }
    let rec = io.copy_file(rel).await?;
    note(log, Step::Copy, &obj, rec.encode())?;
    io.audit(Step::Copy, &obj, Some(&rec), "copied", "").await?;
    Ok(rec)
}

/// Moves one day file into the held folder (rename only, then a sync of
/// both folders). O(1).
///
/// # Errors
/// The target exists already (never overwritten), or the rename failed.
pub fn move_day_file(root: &Path, rel: &Path) -> anyhow::Result<PathBuf> {
    let target = held_target(rel);
    let to = root.join(&target);
    if to.exists() {
        bail!("{} already exists; refusing to overwrite it", to.display());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let from = root.join(rel);
    std::fs::rename(&from, &to)
        .with_context(|| format!("rename {} -> {}", from.display(), to.display()))?;
    for dir in [from.parent(), to.parent()].into_iter().flatten() {
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("sync {}", dir.display()))?;
    }
    Ok(target)
}

/// Steps 1 to 5. Returns the rebuild's report.
///
/// # Errors
/// A guard refused, or any step failed; the progress file and S3 keep what
/// was done and the next run resumes from the results.
// TEST-EXEMPT: covered by test_clean_run_copies_drops_moves_rebuilds_verifies_and_marks_done and proptest_crash_and_resume_over_every_step
pub(crate) async fn run_apply<I: RepairIo>(
    io: &mut I,
    log: &mut ProgressLog,
    root: &Path,
    wal_dir: &Path,
    day: NaiveDate,
) -> anyhow::Result<RebuildReport> {
    let day_text = day.format("%Y-%m-%d").to_string();
    if log.has(Step::Verify, DAY_OBJECT)
        && let Some(rep) = log
            .find(Step::Rebuild, DAY_OBJECT)
            .and_then(|l| RebuildReport::decode(&l.detail))
    {
        info!("day repair: already verified; apply has nothing to do");
        return Ok(rep);
    }
    let pre = io.preflight().await?;
    let waiting = waiting_wal_segments(wal_dir, day);
    let refusals = apply_refusals(&pre, &waiting, &day_text);
    if !refusals.is_empty() {
        bail!("apply refused: {}", refusals.join("; "));
    }
    let hours = day_unix_hours(day)?;
    let files = list_day_files(root, &day_text, hours);
    note(
        log,
        Step::Enumerate,
        DAY_OBJECT,
        format!(
            "partitions={};files={};canonical={};fingerprint_day={}",
            pre.partitions.len(),
            files.len(),
            pre.canonical_objects.len(),
            pre.fingerprints[1]
        ),
    )?;
    io.audit(Step::Enumerate, DAY_OBJECT, None, "listed", "")
        .await?;

    // 2. Copy everything before anything is dropped or moved.
    for p in &pre.partitions {
        ensure_copied(io, log, p).await?;
    }
    for rel in &files {
        ensure_file_copied(io, log, rel).await?;
    }
    // 3. Move the files aside BEFORE any drop (each has its verified copy
    // from step 2; rename only). If the run then stops (a crash, or the
    // window closing), a later app start can no longer replay the day's
    // wrong seals or ticks into the partitions.
    io.window_open().await?;
    for rel in &files {
        let obj = rel.display().to_string();
        if !log.has(Step::Copy, &obj) {
            bail!("no copy recorded for {obj}; not moving it");
        }
        let target = move_day_file(root, rel)?;
        let detail = format!("to={}", target.display());
        note(log, Step::Move, &obj, detail.clone())?;
        io.audit(Step::Move, &obj, None, "moved", &detail).await?;
    }
    // 4. Drop, each against its verified copy, each only inside the window.
    for p in &pre.partitions {
        io.window_open().await?;
        let obj = p.object();
        let rec = log
            .find(Step::Copy, &obj)
            .and_then(|l| CopyRecord::decode(&l.detail))
            .with_context(|| format!("no copy recorded for {obj}"))?;
        if !io.copy_still_verified(&rec).await? {
            bail!("the quarantine copy of {obj} no longer verifies; not dropping it");
        }
        io.drop_partition(p, &rec).await?;
        note(log, Step::Drop, &obj, rec.encode())?;
        io.audit(Step::Drop, &obj, Some(&rec), "dropped", "")
            .await?;
    }
    // A partition an earlier run copied and dropped (the drop landed, the
    // note did not) counts as dropped only while its copy verifies.
    let listed: std::collections::BTreeSet<String> =
        pre.partitions.iter().map(Partition::object).collect();
    let earlier: Vec<(String, CopyRecord)> = log
        .lines()
        .iter()
        .filter(|l| {
            l.step == Step::Copy && l.object.contains('/') && !l.object.starts_with("data/")
        })
        .filter(|l| !listed.contains(&l.object))
        .filter_map(|l| CopyRecord::decode(&l.detail).map(|r| (l.object.clone(), r)))
        .collect();
    for (obj, rec) in earlier {
        if log.has(Step::Drop, &obj) {
            continue;
        }
        if !io.copy_still_verified(&rec).await? {
            bail!("{obj} is gone from QuestDB but its quarantine copy does not verify");
        }
        note(log, Step::Drop, &obj, rec.encode())?;
        io.audit(Step::Drop, &obj, Some(&rec), "already_gone", "")
            .await?;
    }
    let left = io.list_partitions().await?;
    if !left.is_empty() {
        bail!(
            "{} partition(s) still in QuestDB after the drops: {}",
            left.len(),
            left.iter()
                .map(Partition::object)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // 5. Rebuild (idempotent: every row upserts onto itself). The rebuild
    // re-checks the window between segments as well.
    io.window_open().await?;
    let rep = io.rebuild().await?;
    note(log, Step::Rebuild, DAY_OBJECT, rep.encode())?;
    let reasons = rep.unfinished_reasons();
    let outcome = if reasons.is_empty() {
        "rebuilt"
    } else {
        "unfinished"
    };
    io.audit(
        Step::Rebuild,
        DAY_OBJECT,
        None,
        outcome,
        &reasons.join("; "),
    )
    .await?;
    if !reasons.is_empty() {
        bail!("the rebuild is not finished: {}", reasons.join("; "));
    }
    Ok(rep)
}

/// Steps 6 and 7: the verify gate, then `.done`.
///
/// # Errors
/// No rebuild recorded, the gate failed, or the marker could not be written.
pub(crate) async fn run_verify<I: RepairIo>(
    io: &mut I,
    log: &mut ProgressLog,
    root: &Path,
    day: NaiveDate,
    today: NaiveDate,
) -> anyhow::Result<()> {
    let done_dir = root.join(DAY_REPAIR_DIR);
    let done = tickvault_storage::held_days::read_day_marker(&done_dir, MarkerKind::Done, day);
    if log.has(Step::Done, DAY_OBJECT) && matches!(done, MarkerRead::Valid { .. }) {
        info!("day repair: already done");
        return Ok(());
    }
    let rep = log
        .find(Step::Rebuild, DAY_OBJECT)
        .and_then(|l| RebuildReport::decode(&l.detail))
        .context("no rebuild recorded; run apply first")?;
    let unfinished = rep.unfinished_reasons();
    if !unfinished.is_empty() {
        bail!(
            "the recorded rebuild is not finished: {}",
            unfinished.join("; ")
        );
    }
    let findings = io.verify(&rep).await?;
    if !findings.is_empty() {
        io.audit(
            Step::Verify,
            DAY_OBJECT,
            None,
            "failed",
            &findings.join("; "),
        )
        .await?;
        bail!("the verify gate failed: {}", findings.join("; "));
    }
    note(log, Step::Verify, DAY_OBJECT, String::new())?;
    io.audit(Step::Verify, DAY_OBJECT, None, "passed", "")
        .await?;
    tickvault_storage::held_days::write_day_marker(
        &done_dir,
        MarkerKind::Done,
        day,
        today,
        "day repair verified",
    )
    .with_context(|| format!("write the .done marker in {}", done_dir.display()))?;
    note(log, Step::Done, DAY_OBJECT, String::new())?;
    io.audit(Step::Done, DAY_OBJECT, None, "released", "")
        .await?;
    Ok(())
}

/// One line to stdout (the SSM output) and the log.
fn say(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    writeln!(out, "day-repair: {line}").unwrap_or_default();
}

/// Runs `systemctl` with `args` and returns `(success, stdout)`. Cold.
fn systemctl(args: &[&str]) -> (bool, String) {
    match std::process::Command::new("systemctl").args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// Puts the unit hold in place, then stops the app, then checks it reads
/// inactive or failed. Cold.
///
/// # Errors
/// The drop-in could not be written, or the app still reads active.
fn hold_and_stop_app() -> anyhow::Result<()> {
    let dropin = Path::new(UNIT_HOLD_DROPIN);
    if let Some(dir) = dropin.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(
        dropin,
        "[Unit]\n# day-repair (Quote 63): the app does not start while the repair runs.\n\
         ConditionPathExists=/run/tickvault-day-repair-never-exists\n",
    )
    .with_context(|| format!("write {}", dropin.display()))?;
    let (reloaded, out) = systemctl(&["daemon-reload"]);
    if !reloaded {
        bail!("systemctl daemon-reload failed: {out}");
    }
    let (_, out) = systemctl(&["stop", APP_UNIT]);
    app_stopped().map_err(|e| anyhow::anyhow!("{e} ({out})"))
}

/// Whether the app reads stopped (`inactive` or `failed`). Cold.
///
/// # Errors
/// Any other state.
fn app_stopped() -> anyhow::Result<()> {
    let (_, state) = systemctl(&["is-active", APP_UNIT]);
    if state == "inactive" || state == "failed" {
        Ok(())
    } else {
        bail!("the app unit reads {state:?}, not stopped")
    }
}

/// Removes the unit hold (the app is left stopped; the curfew stops the box).
fn release_app_unit() {
    match std::fs::remove_file(UNIT_HOLD_DROPIN) {
        Ok(()) => {
            let (ok, out) = systemctl(&["daemon-reload"]);
            if !ok {
                warn!(
                    code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                    source = "day_repair_release",
                    out = %out,
                    "day repair: daemon-reload after removing the unit hold failed; a reboot \
                     clears the hold"
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_release",
            error = %e,
            "day repair: could not remove the unit hold; a reboot clears it"
        ),
    }
}

/// Writes `/tickvault/<env>/keep-alive-until` so the hourly curfew does not
/// stop the box mid-run: `now + hours`, never past Monday 23 Nov 07:30 IST
/// (before the 08:30 start). Best effort: a failure is logged, the run goes
/// on (the runner keeps the box up too). Cold.
async fn write_keep_alive(env: &str, hours: i64) {
    let latest = NaiveDateTime::parse_from_str("2026-11-23T07:30:00", "%Y-%m-%dT%H:%M:%S").ok();
    let until = now_ist() + chrono::Duration::hours(hours);
    let until = latest.map_or(until, |l| until.min(l));
    let value = until.format("%Y-%m-%dT%H:%M:%S").to_string();
    let ssm = tickvault_core::auth::secret_manager::create_ssm_client_public().await;
    let put = ssm
        .put_parameter()
        .name(format!("/tickvault/{env}/keep-alive-until"))
        .value(&value)
        .r#type(aws_sdk_ssm::types::ParameterType::String)
        .overwrite(true)
        .send();
    match tokio::time::timeout(KEEP_ALIVE_PUT_TIMEOUT, put).await {
        Ok(Ok(_)) => say(&format!("keep-alive-until set to {value} IST")),
        Ok(Err(e)) => warn!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_keep_alive",
            error = %aws_sdk_ssm::error::DisplayErrorContext(&e),
            "day repair: could not write keep-alive-until"
        ),
        Err(_) => warn!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_keep_alive",
            "day repair: writing keep-alive-until timed out"
        ),
    }
}

/// Gives every path under `paths` (recursively) the owner of `data/`, so the
/// app (not root, which runs the SSM command) can read and move them later.
#[cfg(unix)]
fn chown_like_data(root: &Path, paths: &[PathBuf]) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(meta) = std::fs::metadata(root.join("data")) else {
        return;
    };
    let (uid, gid) = (meta.uid(), meta.gid());
    fn walk(p: &Path, uid: u32, gid: u32) {
        if std::os::unix::fs::chown(p, Some(uid), Some(gid)).is_err() {
            return;
        }
        if p.is_dir()
            && let Ok(rd) = std::fs::read_dir(p)
        {
            for e in rd.flatten() {
                walk(&e.path(), uid, gid);
            }
        }
    }
    for p in paths {
        let full = root.join(p);
        if full.exists() {
            walk(&full, uid, gid);
        }
    }
}

#[cfg(not(unix))]
fn chown_like_data(_root: &Path, _paths: &[PathBuf]) {}

/// Entry point, called from `main` before any other boot step. Returns the
/// process exit code; never panics.
// TEST-EXEMPT: box entry point (systemctl, the WAL lock, AWS); the guards and the step driver it calls are unit- and property-tested.
pub async fn run_cli(args: &[String]) -> i32 {
    // The coded lines go to stdout, the SSM command output. A subscriber
    // already installed (none is, this early in `main`) keeps its own.
    if tracing_subscriber::fmt()
        .with_writer(std::io::stdout)
        .with_target(false)
        .try_init()
        .is_err()
    {
        say("a log subscriber was already installed; keeping it");
    }
    let (mode, sha) = match parse_args(args) {
        Ok(v) => v,
        Err(e) => {
            say(&format!(
                "usage: tickvault day-repair <dry|apply|verify> <sha>: {e}"
            ));
            return EXIT_USAGE;
        }
    };
    say(&format!("{} for {REPAIR_DAY}, binary {sha}", mode.as_str()));
    match run_mode(mode).await {
        Ok(code) => code,
        Err(e) => {
            error!(
                code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
                source = "day_repair_failed",
                mode = mode.as_str(),
                error = %format!("{e:#}"),
                "day repair FAILED; the day stays held and a re-run resumes from the results"
            );
            say(&format!("FAILED: {e:#}"));
            EXIT_FAILED
        }
    }
}

/// The guards, then the mode. `Ok(code)` for a refusal (exit 3) or success.
async fn run_mode(mode: Mode) -> anyhow::Result<i32> {
    std::env::set_current_dir(APP_DIR).with_context(|| format!("cd {APP_DIR}"))?;
    let root = PathBuf::from(".");
    let env = crate::boot_helpers::resolve_config_env();
    let config: tickvault_common::config::ApplicationConfig =
        crate::boot_helpers::layered_config_figment(&env)
            .extract()
            .context("load the configuration")?;
    let calendar =
        tickvault_common::trading_calendar::TradingCalendar::from_config(&config.trading)?;
    let now = now_ist();
    if let Some(why) = window_refusal(now, calendar.is_session_day(now.date())) {
        say(&format!("REFUSED: {why}"));
        return Ok(EXIT_REFUSED);
    }
    let day = repair_day()?;
    let wal_dir = crate::boot_helpers::ws_wal_dir();
    let staging = PathBuf::from(STAGING_DIR);
    let mut io = prod_io::ProdIo::new(&config, &root, &staging).await?;
    if mode == Mode::Dry {
        return dry_report(&mut io, &root, &wal_dir, day).await;
    }
    if mode == Mode::Apply {
        hold_and_stop_app()?;
    } else {
        app_stopped()?;
    }
    let Some(_wal_guard) = tickvault_storage::ws_frame_spill::lock_wal_dir(&wal_dir)
        .context("another process owns the WAL folder")?
    else {
        say("REFUSED: the WAL folder lock could not be taken (unwritable folder)");
        return Ok(EXIT_REFUSED);
    };
    write_keep_alive(&env, if mode == Mode::Apply { 8 } else { 3 }).await;
    let progress_dir = root.join(DAY_REPAIR_DIR);
    let mut log = ProgressLog::open(&ProgressLog::path_for(&progress_dir, REPAIR_DAY))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if log.had_torn_tail() {
        say(
            "the progress file ended in a torn line (a crash mid-write); it is ignored and rewritten",
        );
    }
    let owned = [
        PathBuf::from(DAY_REPAIR_DIR),
        PathBuf::from(HELD_FILES_DIR),
        PathBuf::from("data/spill"),
        PathBuf::from("data/dlq"),
    ];
    let result = match mode {
        Mode::Apply => run_apply(&mut io, &mut log, &root, &wal_dir, day)
            .await
            .map(|rep| {
                say(&format!("apply finished: {}", rep.encode()));
            }),
        Mode::Verify => run_verify(&mut io, &mut log, &root, day, now.date()).await,
        Mode::Dry => Ok(()),
    };
    // The staging folder holds nothing after a clean run; a failed one may
    // leave a segment, removed here (only files this run created live there).
    if std::fs::remove_dir_all(&staging).is_err() && staging.exists() {
        warn!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_staging",
            "day repair: could not remove the staging folder {}",
            staging.display()
        );
    }
    chown_like_data(&root, &owned);
    if let Err(ref e) = result
        && format!("{e:#}").contains(WINDOW_STOP)
    {
        // The window closed mid-run: the files are already aside (moved
        // before any drop), so the app may start at its usual time. The day
        // stays held; a run in the next window resumes from the results.
        release_app_unit();
        say(&format!(
            "STOPPED: {e:#}; the app unit is released so the app starts on time, the day stays \
             held, re-run in the next window to resume"
        ));
        return Ok(EXIT_REFUSED);
    }
    result?;
    if mode == Mode::Verify {
        release_app_unit();
        write_keep_alive(&env, 0).await;
        say("verify passed; .done written, the app unit released (left stopped)");
    }
    Ok(EXIT_OK)
}

/// `dry`: every guard and object, printed; nothing changes. Exit 3 when
/// `apply` would refuse.
async fn dry_report(
    io: &mut prod_io::ProdIo,
    root: &Path,
    wal_dir: &Path,
    day: NaiveDate,
) -> anyhow::Result<i32> {
    let day_text = day.format("%Y-%m-%d").to_string();
    let pre = io.preflight().await?;
    let waiting = waiting_wal_segments(wal_dir, day);
    let files = list_day_files(root, &day_text, day_unix_hours(day)?);
    for p in &pre.partitions {
        say(&format!("partition {}", p.object()));
    }
    for f in &files {
        say(&format!("file {}", f.display()));
    }
    for k in &pre.canonical_objects {
        say(&format!(
            "canonical object kept (the export adds a .sha- copy): {k}"
        ));
    }
    say(&format!(
        "fingerprints (index ticks before 09:00) 19/20/21 Nov: {:?}; Brutex DONE: {}",
        pre.fingerprints, pre.brutex_done
    ));
    let refusals = apply_refusals(&pre, &waiting, &day_text);
    if refusals.is_empty() {
        say("dry run: apply may go ahead");
        Ok(EXIT_OK)
    } else {
        for r in &refusals {
            say(&format!("apply would refuse: {r}"));
        }
        Ok(EXIT_REFUSED)
    }
}

/// The production [`RepairIo`]: the ONE place this module names the storage
/// half (`partition_archive::quarantine_copy`, `day_repair_audit_persistence`).
/// The quarantine copy writes its own `day_repair_audit` rows for each copy
/// and drop; this adapter writes the rows of the other steps.
mod prod_io {
    use std::path::{Path, PathBuf};

    use anyhow::{Context as _, bail};
    use chrono::NaiveDate;
    use tickvault_common::config::ApplicationConfig;
    use tickvault_storage::day_repair_audit_persistence::{
        DayRepairAuditRow, DayRepairAuditWriter, DayRepairOutcome, DayRepairStep,
        ensure_day_repair_audit_table,
    };
    use tickvault_storage::partition_archive::PartitionArchiver;
    use tickvault_storage::partition_archive::quarantine_copy::{
        QuarantineMode, QuarantineOutcome, QuarantinedObject, quarantine_tables,
    };

    use super::rebuild::{self, RebuildReport};
    use super::state::Step;
    use super::{CopyRecord, Partition, Preflight, REPAIR_DAY, RepairIo};

    pub(super) struct ProdIo {
        config: ApplicationConfig,
        root: PathBuf,
        staging: PathBuf,
        archiver: PartitionArchiver,
        audit: DayRepairAuditWriter,
        run_id: String,
        s3: aws_sdk_s3::Client,
        bucket: String,
        client: reqwest::Client,
        exec_url: String,
        day: NaiveDate,
    }

    /// Lowercase hex to bytes; `None` on an odd length or a bad digit. Pure.
    pub(super) fn hex_decode(hex: &str) -> Option<Vec<u8>> {
        if hex.len() % 2 != 0 {
            return None;
        }
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect()
    }

    /// Standard base64 with padding (S3's `x-amz-checksum-sha256` form).
    /// Pure, O(bytes).
    pub(super) fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn to_record(c: &QuarantinedObject) -> CopyRecord {
        CopyRecord {
            key: c.s3_key.clone(),
            sha256: c.sha256_hex.clone(),
            rows: c.rows.unwrap_or(0),
            bytes: c.bytes,
        }
    }

    fn outcome_error(what: &str, o: QuarantineOutcome) -> anyhow::Error {
        match o {
            QuarantineOutcome::Refused { reason } => {
                anyhow::anyhow!("{what}: refused ({reason})")
            }
            QuarantineOutcome::Kept { stage, detail } => {
                anyhow::anyhow!("{what}: kept at {stage}: {detail}")
            }
            QuarantineOutcome::Copied(c) => {
                anyhow::anyhow!("{what}: copied to {} but not dropped", c.s3_key)
            }
            QuarantineOutcome::Dropped(c) => {
                anyhow::anyhow!("{what}: dropped unexpectedly (copy {})", c.s3_key)
            }
        }
    }

    fn audit_step(step: Step) -> DayRepairStep {
        match step {
            Step::Enumerate => DayRepairStep::Enumerate,
            Step::Copy => DayRepairStep::Copy,
            Step::Drop => DayRepairStep::Drop,
            Step::Move => DayRepairStep::MoveFiles,
            Step::Rebuild => DayRepairStep::Rebuild,
            Step::Verify => DayRepairStep::Verify,
            Step::Done => DayRepairStep::Done,
        }
    }

    fn audit_outcome(outcome: &str) -> DayRepairOutcome {
        match outcome {
            "failed" | "unfinished" => DayRepairOutcome::Failed,
            "copied" => DayRepairOutcome::Copied,
            "dropped" => DayRepairOutcome::Dropped,
            _ => DayRepairOutcome::Ok,
        }
    }

    impl ProdIo {
        pub(super) async fn new(
            config: &ApplicationConfig,
            root: &Path,
            staging: &Path,
        ) -> anyhow::Result<Self> {
            if !ensure_day_repair_audit_table(&config.questdb).await {
                bail!("day_repair_audit could not be created with its DEDUP key");
            }
            let archiver = PartitionArchiver::new(&config.questdb, &config.partition_retention)
                .await?
                .context("no cold bucket resolves (TV_ENVIRONMENT / archive_bucket)")?;
            let bucket = tickvault_storage::s3_cold::S3Cold::load(
                &config.partition_retention.archive_bucket,
            )
            .await
            .context("no cold bucket resolves")?
            .bucket()
            .to_owned();
            let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .load()
                .await;
            let client = tickvault_storage::http_client::build_probe_client(120)
                .map_err(|e| anyhow::anyhow!("http client: {e}"))?;
            Ok(Self {
                config: config.clone(),
                root: root.to_path_buf(),
                staging: staging.to_path_buf(),
                archiver,
                audit: DayRepairAuditWriter::new(&config.questdb),
                run_id: format!("day-repair-{}", chrono::Utc::now().timestamp()),
                s3: aws_sdk_s3::Client::new(&aws),
                bucket,
                client,
                exec_url: format!(
                    "http://{}:{}/exec",
                    config.questdb.host, config.questdb.http_port
                ),
                day: super::repair_day()?,
            })
        }

        /// `(length, stored SHA-256 base64)` at `key`, `None` when absent.
        /// One HeadObject (read only).
        async fn head(&self, key: &str) -> anyhow::Result<Option<(u64, Option<String>)>> {
            match self
                .s3
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
                .send()
                .await
            {
                Ok(h) => Ok(Some((
                    h.content_length()
                        .and_then(|n| u64::try_from(n).ok())
                        .unwrap_or(0),
                    h.checksum_sha256().map(|s| s.trim().to_owned()),
                ))),
                Err(e) if e.as_service_error().is_some_and(|se| se.is_not_found()) => Ok(None),
                Err(e) => bail!(
                    "S3 HeadObject {key}: {}",
                    aws_sdk_s3::error::DisplayErrorContext(&e)
                ),
            }
        }
    }

    impl RepairIo for ProdIo {
        async fn preflight(&mut self) -> anyhow::Result<Preflight> {
            let partitions = self.list_partitions().await?;
            let mut canonical_objects = Vec::new();
            for p in &partitions {
                if self
                    .archiver
                    .canonical_object(&p.table, &p.partition)
                    .await?
                    .is_some()
                {
                    canonical_objects.push(p.object());
                }
            }
            let done_key = tickvault_storage::partition_archive::daily_export::done_key(self.day);
            let brutex_done = self.head(&done_key).await?.is_some();
            let mut fingerprints = [0u64; 3];
            for (i, d) in [self.day.pred_opt(), Some(self.day), self.day.succ_opt()]
                .into_iter()
                .enumerate()
            {
                let d = d.context("neighbour day")?;
                fingerprints[i] =
                    rebuild::offset_fingerprint(&self.client, &self.exec_url, d).await?;
            }
            Ok(Preflight {
                partitions,
                canonical_objects,
                brutex_done,
                fingerprints,
            })
        }

        async fn list_partitions(&mut self) -> anyhow::Result<Vec<Partition>> {
            let mut out = Vec::new();
            for table in quarantine_tables() {
                for (partition, _size) in self.archiver.repair_day_partitions(table).await? {
                    out.push(Partition {
                        table: table.to_owned(),
                        partition,
                    });
                }
            }
            out.sort();
            Ok(out)
        }

        async fn copy_partition(&mut self, p: &Partition) -> anyhow::Result<CopyRecord> {
            match self
                .archiver
                .quarantine_partition(
                    &p.table,
                    &p.partition,
                    QuarantineMode::CopyOnly,
                    &mut self.audit,
                    &self.run_id,
                )
                .await
            {
                QuarantineOutcome::Copied(c) => Ok(to_record(&c)),
                other => Err(outcome_error(&format!("copy {}", p.object()), other)),
            }
        }

        async fn copy_still_verified(&mut self, rec: &CopyRecord) -> anyhow::Result<bool> {
            let Some(want) = hex_decode(&rec.sha256).map(|b| base64(&b)) else {
                return Ok(false);
            };
            Ok(self
                .head(&rec.key)
                .await?
                .is_some_and(|(len, sha)| len == rec.bytes && sha.as_deref() == Some(&want)))
        }

        async fn drop_partition(&mut self, p: &Partition, rec: &CopyRecord) -> anyhow::Result<()> {
            // Copy-and-drop re-exports and re-verifies right before the drop
            // (an identical object is reused), so the drop never trusts the
            // earlier copy alone.
            match self
                .archiver
                .quarantine_partition(
                    &p.table,
                    &p.partition,
                    QuarantineMode::CopyAndDrop,
                    &mut self.audit,
                    &self.run_id,
                )
                .await
            {
                QuarantineOutcome::Dropped(c) => {
                    if c.s3_key != rec.key {
                        tracing::warn!(
                            code = tickvault_common::error_code::ErrorCode::DhanLtt02RecordUnsettled
                                .code_str(),
                            source = "day_repair_copy_moved",
                            object = %p.object(),
                            first = %rec.key,
                            second = %c.s3_key,
                            "day repair: the copy before the drop landed at a different key \
                             (different bytes); both copies are kept"
                        );
                    }
                    Ok(())
                }
                other => Err(outcome_error(&format!("drop {}", p.object()), other)),
            }
        }

        async fn copy_file(&mut self, rel: &Path) -> anyhow::Result<CopyRecord> {
            let local = self.root.join(rel);
            let name = rel.display().to_string();
            match self
                .archiver
                .quarantine_local_file(&local, &name, &mut self.audit, &self.run_id)
                .await
            {
                QuarantineOutcome::Copied(c) => Ok(to_record(&c)),
                other => Err(outcome_error(&format!("copy {name}"), other)),
            }
        }

        async fn rebuild(&mut self) -> anyhow::Result<RebuildReport> {
            rebuild::rebuild_day(&self.config, self.day, &self.root, &self.staging).await
        }

        async fn window_open(&mut self) -> anyhow::Result<()> {
            super::window_open_now(&self.config)
        }

        async fn verify(&mut self, rep: &RebuildReport) -> anyhow::Result<Vec<String>> {
            let mut findings = rebuild::verify_day(&self.config, self.day, rep).await?;
            let done_key = tickvault_storage::partition_archive::daily_export::done_key(self.day);
            let brutex_done = self.head(&done_key).await?.is_some();
            findings.extend(super::done_holds_release(brutex_done, REPAIR_DAY));
            Ok(findings)
        }

        async fn audit(
            &mut self,
            step: Step,
            object: &str,
            rec: Option<&CopyRecord>,
            outcome: &str,
            detail: &str,
        ) -> anyhow::Result<()> {
            // The quarantine copy already recorded each copy and drop it made.
            if matches!(
                (step, outcome),
                (Step::Copy, "copied") | (Step::Drop, "dropped")
            ) {
                return Ok(());
            }
            let mut row = DayRepairAuditRow::now(
                REPAIR_DAY,
                audit_step(step),
                object,
                audit_outcome(outcome),
                &self.run_id,
            );
            if let Some(r) = rec {
                row.row_count = Some(r.rows);
                row.byte_count = Some(r.bytes);
                row.sha256 = r.sha256.clone();
                row.s3_key = r.key.clone();
            }
            row.detail = if detail.is_empty() {
                outcome.to_owned()
            } else {
                format!("{outcome}: {detail}")
            };
            self.audit.append_and_flush(&row)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SCRATCH: AtomicU64 = AtomicU64::new(0);

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tv-day-repair-{name}-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn nov20() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 20).expect("date")
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn test_parse_args_accepts_the_three_modes_and_refuses_bad_input() {
        assert!(is_invocation(&args(&["tickvault", "day-repair"])));
        assert!(!is_invocation(&args(&["tickvault", "ensure-questdb"])));
        for (m, want) in [
            ("dry", Mode::Dry),
            ("apply", Mode::Apply),
            ("verify", Mode::Verify),
        ] {
            assert_eq!(
                parse_args(&args(&["tickvault", "day-repair", m, SHA])),
                Ok((want, SHA.to_owned()))
            );
        }
        assert!(parse_args(&args(&["tickvault", "day-repair", "wipe", SHA])).is_err());
        assert!(parse_args(&args(&["tickvault", "day-repair", "dry", "abc"])).is_err());
        assert!(
            parse_args(&args(&[
                "tickvault",
                "day-repair",
                "dry",
                &SHA.to_uppercase()
            ]))
            .is_err()
        );
        assert!(parse_args(&args(&["tickvault", "day-repair", "dry", SHA, "x"])).is_err());
        assert!(parse_args(&args(&["tickvault", "day-repair"])).is_err());
    }

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").expect("time")
    }

    #[test]
    fn test_window_refuses_0800_to_1630_on_a_trading_day_and_after_2026_11_30() {
        assert!(window_refusal(at("2026-11-23T08:00:00"), true).is_some());
        assert!(window_refusal(at("2026-11-23T16:29:59"), true).is_some());
        assert!(window_refusal(at("2026-11-23T07:59:59"), true).is_none());
        assert!(window_refusal(at("2026-11-23T16:30:00"), true).is_none());
        // Sunday 22 Nov is not a session day: any hour.
        assert!(window_refusal(at("2026-11-22T11:00:00"), false).is_none());
        assert!(window_refusal(at("2026-11-30T23:59:59"), false).is_none());
        assert!(window_refusal(at("2026-12-01T00:00:00"), false).is_some());
    }

    #[test]
    fn test_tick_spill_hour_and_day_file_classification() {
        assert_eq!(tick_spill_hour("ticks-dhan-498650.ilp"), Some(498_650));
        assert_eq!(
            tick_spill_hour("held-feed_aux_packets-replay-dhan-498650.ilp.offset"),
            Some(498_650)
        );
        assert_eq!(tick_spill_hour("ticks-dhan.ilp"), None);
        let hours = day_unix_hours(nov20()).expect("hours");
        // 20 Nov 00:00 IST = 19 Nov 18:30 UTC.
        assert_eq!(hours.0, 1_795_113_000 / 3_600);
        assert_eq!(hours.1, (1_795_199_400 - 1) / 3_600);
        let day = REPAIR_DAY;
        let yes = [
            "data/spill/seals_v4-2026-11-20.bin",
            "data/spill/seals_v4-2026-11-20.bin.1",
            "data/spill/replaying/seals_v4-2026-11-20.bin",
            "data/dlq/seals_v4-2026-11-20.ndjson",
        ];
        for p in yes {
            assert!(is_day_file(Path::new(p), day, hours), "{p}");
        }
        let in_day = format!("data/spill/ticks/ticks-dhan-{}.ilp", hours.0 + 3);
        assert!(is_day_file(Path::new(&in_day), day, hours));
        let no = [
            "data/spill/seals_v4-2026-11-19.bin".to_owned(),
            "data/spill/refused/seals_v4-2026-11-20.bin".to_owned(),
            "data/spill/archive/seals_v4-2026-11-20.bin".to_owned(),
            "data/spill/audit/order_audit/x".to_owned(),
            "data/spill/depth/depth-dhan-498650.ilp".to_owned(),
            format!("data/spill/ticks/ticks-dhan-{}.ilp", hours.1 + 1),
            format!("data/spill/ticks/quarantine/ticks-dhan-{}.ilp", hours.0),
        ];
        for p in &no {
            assert!(!is_day_file(Path::new(p), day, hours), "{p}");
        }
        assert_eq!(
            held_target(Path::new("data/spill/ticks/a.ilp")),
            PathBuf::from("data/held/2026-11-20/spill/ticks/a.ilp")
        );
    }

    #[test]
    fn test_allowlist_is_held_kinds_of_2026_11_20_only() {
        let p = |t: &str, part: &str| Partition {
            table: t.to_owned(),
            partition: part.to_owned(),
        };
        assert!(p("ticks", "2026-11-20T09").allowed(REPAIR_DAY));
        assert!(p("tick_quotes", "2026-11-20T15").allowed(REPAIR_DAY));
        assert!(p("feed_aux_packets", "2026-11-20T00").allowed(REPAIR_DAY));
        assert!(p("candles_1m", "2026-11-20").allowed(REPAIR_DAY));
        assert!(!p("market_depth", "2026-11-20T09").allowed(REPAIR_DAY));
        assert!(!p("market_depth_book", "2026-11-20T09").allowed(REPAIR_DAY));
        assert!(!p("order_audit", "2026-11-20").allowed(REPAIR_DAY));
        assert!(!p("dhan_live_crossverify_audit", "2026-11-20").allowed(REPAIR_DAY));
        assert!(!p("ticks", "2026-11-21T00").allowed(REPAIR_DAY));
        assert!(!p("ticks", "2026-11-19T15").allowed(REPAIR_DAY));
        assert!(!p("candles_1m", "2026-11-2").allowed(REPAIR_DAY));
        assert!(!p("candles_1m", "2026-11-200").allowed(REPAIR_DAY));
    }

    #[test]
    fn test_hex_and_base64_helpers_match_the_s3_checksum_form() {
        assert_eq!(prod_io::base64(b""), "");
        assert_eq!(prod_io::base64(b"f"), "Zg==");
        assert_eq!(prod_io::base64(b"fo"), "Zm8=");
        assert_eq!(prod_io::base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(prod_io::hex_decode("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(prod_io::hex_decode("0"), None);
        assert_eq!(prod_io::hex_decode("zz"), None);
    }

    #[test]
    fn test_copy_record_round_trips() {
        let r = CopyRecord {
            key: "quarantine/2026-11-20-ltt-utc-shift/ticks/2026-11-20T09.csv.gz".to_owned(),
            sha256: "ab".repeat(32),
            rows: 12,
            bytes: 345,
        };
        assert_eq!(CopyRecord::decode(&r.encode()), Some(r));
        assert_eq!(CopyRecord::decode("key=x;rows=1"), None);
    }

    #[test]
    fn test_apply_refusals_name_each_guard_and_brutex_done_present_refuses_apply() {
        let clean = Preflight {
            partitions: vec![Partition {
                table: "ticks".to_owned(),
                partition: "2026-11-20T09".to_owned(),
            }],
            canonical_objects: vec!["questdb-partitions/ticks/2026-11-20T09.csv.gz".to_owned()],
            brutex_done: false,
            fingerprints: [0, 9_000, 0],
        };
        assert!(apply_refusals(&clean, &[], REPAIR_DAY).is_empty());
        let done = Preflight {
            brutex_done: true,
            ..clean.clone()
        };
        assert_eq!(apply_refusals(&done, &[], REPAIR_DAY).len(), 1);
        assert!(done_holds_release(true, REPAIR_DAY).is_some());
        assert!(done_holds_release(false, REPAIR_DAY).is_none());
        let shifted = Preflight {
            fingerprints: [3, 9_000, 4],
            ..clean.clone()
        };
        assert_eq!(apply_refusals(&shifted, &[], REPAIR_DAY).len(), 2);
        let foreign = Preflight {
            partitions: vec![Partition {
                table: "market_depth".to_owned(),
                partition: "2026-11-20T09".to_owned(),
            }],
            ..clean.clone()
        };
        assert_eq!(apply_refusals(&foreign, &[], REPAIR_DAY).len(), 1);
        assert_eq!(
            apply_refusals(&clean, &[PathBuf::from("data/ws_wal/x.wal")], REPAIR_DAY).len(),
            1
        );
    }

    #[test]
    fn test_waiting_wal_segments_finds_only_the_days_segments() {
        let root = scratch("wal");
        let wal = root.join("ws_wal");
        std::fs::create_dir_all(wal.join("replaying")).expect("dir");
        let (start, _) = rebuild::day_receipt_bounds_nanos(nov20()).expect("bounds");
        let on = format!("ws-frames-{}.wal", start + 3_600_000_000_000);
        let before = format!("ws-frames-{}.wal", start - 1);
        std::fs::write(wal.join(&before), b"").expect("seg");
        std::fs::write(wal.join("replaying").join(&on), b"").expect("seg");
        std::fs::write(wal.join("other.txt"), b"").expect("file");
        let got = waiting_wal_segments(&wal, nov20());
        assert_eq!(got, vec![wal.join("replaying").join(&on)]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_move_day_file_never_overwrites() {
        let root = scratch("move");
        let rel = PathBuf::from("data/spill/seals_v4-2026-11-20.bin");
        std::fs::create_dir_all(root.join("data/spill")).expect("dir");
        std::fs::write(root.join(&rel), b"seal").expect("file");
        let target = move_day_file(&root, &rel).expect("moved");
        assert_eq!(std::fs::read(root.join(&target)).expect("read"), b"seal");
        assert!(!root.join(&rel).exists());
        std::fs::write(root.join(&rel), b"again").expect("file");
        assert!(move_day_file(&root, &rel).is_err());
        assert_eq!(std::fs::read(root.join(&rel)).expect("kept"), b"again");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The remote side the fake IO stands for: QuestDB partitions and S3
    /// copies, shared across "processes" (runs).
    #[derive(Default)]
    struct Remote {
        partitions: BTreeSet<Partition>,
        copies: BTreeMap<String, CopyRecord>,
        violations: Vec<String>,
        rebuilds: u32,
        verifies: u32,
    }

    struct FakeIo<'a> {
        remote: &'a RefCell<Remote>,
        root: PathBuf,
        fail_at: Option<u32>,
        calls: u32,
    }

    impl FakeIo<'_> {
        fn tick(&mut self) -> anyhow::Result<()> {
            self.calls += 1;
            if self.fail_at == Some(self.calls) {
                bail!("injected crash at call {}", self.calls);
            }
            Ok(())
        }
    }

    impl RepairIo for FakeIo<'_> {
        async fn preflight(&mut self) -> anyhow::Result<Preflight> {
            self.tick()?;
            Ok(Preflight {
                partitions: self.list_partitions().await?,
                ..Preflight::default()
            })
        }
        async fn list_partitions(&mut self) -> anyhow::Result<Vec<Partition>> {
            self.tick()?;
            Ok(self.remote.borrow().partitions.iter().cloned().collect())
        }
        async fn copy_partition(&mut self, p: &Partition) -> anyhow::Result<CopyRecord> {
            self.tick()?;
            let mut r = self.remote.borrow_mut();
            if !r.partitions.contains(p) {
                r.violations
                    .push(format!("copy of a missing {}", p.object()));
            }
            let rec = CopyRecord {
                key: format!("q/{}", p.object()),
                sha256: "aa".to_owned(),
                rows: 1,
                bytes: 1,
            };
            r.copies.insert(rec.key.clone(), rec.clone());
            drop(r);
            self.tick()?;
            Ok(rec)
        }
        async fn copy_still_verified(&mut self, rec: &CopyRecord) -> anyhow::Result<bool> {
            self.tick()?;
            Ok(self.remote.borrow().copies.get(&rec.key) == Some(rec))
        }
        async fn drop_partition(&mut self, p: &Partition, rec: &CopyRecord) -> anyhow::Result<()> {
            self.tick()?;
            let mut r = self.remote.borrow_mut();
            if r.copies.get(&format!("q/{}", p.object())) != Some(rec) {
                r.violations
                    .push(format!("drop of {} without its copy", p.object()));
            }
            r.partitions.remove(p);
            drop(r);
            self.tick()
        }
        async fn copy_file(&mut self, rel: &Path) -> anyhow::Result<CopyRecord> {
            self.tick()?;
            let bytes = std::fs::read(self.root.join(rel))?;
            let rec = CopyRecord {
                key: format!("q/files/{}", rel.display()),
                sha256: format!("{:08x}", state::crc32_ieee(&bytes)),
                rows: 0,
                bytes: bytes.len() as u64,
            };
            self.remote
                .borrow_mut()
                .copies
                .insert(rec.key.clone(), rec.clone());
            self.tick()?;
            Ok(rec)
        }
        async fn rebuild(&mut self) -> anyhow::Result<RebuildReport> {
            self.tick()?;
            let hours = day_unix_hours(nov20())?;
            let mut r = self.remote.borrow_mut();
            if !r.partitions.is_empty() {
                r.violations
                    .push("rebuild before every partition was dropped".to_owned());
            }
            if !list_day_files(&self.root, REPAIR_DAY, hours).is_empty() {
                r.violations
                    .push("rebuild before every file was moved".to_owned());
            }
            r.rebuilds += 1;
            drop(r);
            self.tick()?;
            Ok(RebuildReport {
                frames: 1,
                refolded: 1,
                ..RebuildReport::default()
            })
        }
        async fn window_open(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn verify(&mut self, _rep: &RebuildReport) -> anyhow::Result<Vec<String>> {
            self.tick()?;
            self.remote.borrow_mut().verifies += 1;
            Ok(Vec::new())
        }
        async fn audit(
            &mut self,
            _step: Step,
            _object: &str,
            _rec: Option<&CopyRecord>,
            _outcome: &str,
            _detail: &str,
        ) -> anyhow::Result<()> {
            self.tick()
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    fn setup(n_parts: usize, n_files: usize) -> (PathBuf, RefCell<Remote>, Vec<PathBuf>) {
        let root = scratch("resume");
        let hours = day_unix_hours(nov20()).expect("hours");
        std::fs::create_dir_all(root.join("data/spill/ticks")).expect("dir");
        let mut files = Vec::new();
        for i in 0..n_files {
            let rel = if i % 2 == 0 {
                PathBuf::from(format!(
                    "data/spill/ticks/ticks-dhan-{}.ilp",
                    hours.0 + i as u64
                ))
            } else {
                PathBuf::from(format!("data/spill/seals_v4-2026-11-20.bin.{i}"))
            };
            std::fs::write(root.join(&rel), format!("payload {i}")).expect("file");
            files.push(rel);
        }
        let mut remote = Remote::default();
        for i in 0..n_parts {
            remote.partitions.insert(Partition {
                table: if i == 0 {
                    "candles_1m".to_owned()
                } else {
                    "ticks".to_owned()
                },
                partition: if i == 0 {
                    REPAIR_DAY.to_owned()
                } else {
                    format!("{REPAIR_DAY}T{:02}", 8 + i)
                },
            });
        }
        (root, RefCell::new(remote), files)
    }

    /// Runs apply then verify, with one injected crash per process for the
    /// first `crashes` processes, until both succeed. Returns the runs used.
    fn drive(root: &Path, remote: &RefCell<Remote>, crash_points: &[u32]) -> u32 {
        let day = nov20();
        let wal = root.join("data/ws_wal");
        let progress = ProgressLog::path_for(&root.join(DAY_REPAIR_DIR), REPAIR_DAY);
        for run in 0..40u32 {
            let mut io = FakeIo {
                remote,
                root: root.to_path_buf(),
                fail_at: crash_points.get(run as usize).copied(),
                calls: 0,
            };
            let mut log = ProgressLog::open(&progress).expect("progress");
            let ok = block_on(async {
                run_apply(&mut io, &mut log, root, &wal, day).await?;
                run_verify(&mut io, &mut log, root, day, day.succ_opt().unwrap_or(day)).await
            });
            if ok.is_ok() {
                return run + 1;
            }
        }
        panic!("never finished");
    }

    #[test]
    fn test_clean_run_copies_drops_moves_rebuilds_verifies_and_marks_done() {
        let (root, remote, files) = setup(3, 2);
        assert_eq!(drive(&root, &remote, &[]), 1);
        let r = remote.borrow();
        assert!(r.violations.is_empty(), "{:?}", r.violations);
        assert!(r.partitions.is_empty());
        assert_eq!(r.copies.len(), 5);
        assert_eq!(r.rebuilds, 1);
        assert_eq!(r.verifies, 1);
        for f in &files {
            assert!(!root.join(f).exists());
            assert!(root.join(held_target(f)).exists());
        }
        assert!(matches!(
            tickvault_storage::held_days::read_day_marker(
                &root.join(DAY_REPAIR_DIR),
                MarkerKind::Done,
                nov20()
            ),
            MarkerRead::Valid { .. }
        ));
        // A second run is a no-op.
        drop(r);
        assert_eq!(drive(&root, &remote, &[]), 1);
        assert_eq!(remote.borrow().rebuilds, 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_verify_without_a_rebuild_refuses() {
        let root = scratch("noverify");
        let remote = RefCell::new(Remote::default());
        let mut io = FakeIo {
            remote: &remote,
            root: root.clone(),
            fail_at: None,
            calls: 0,
        };
        let mut log = ProgressLog::open(&ProgressLog::path_for(&root, REPAIR_DAY)).expect("log");
        let day = nov20();
        assert!(block_on(run_verify(&mut io, &mut log, &root, day, day)).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// Crash at any call of any step, up to three times, and the run
            /// still ends with every partition dropped only after its verified
            /// copy, every file moved with its bytes, one verified `.done`, and
            /// the rebuild only ever after every drop and move.
            #[test]
            fn proptest_crash_and_resume_over_every_step(
                n_parts in 0usize..5,
                n_files in 0usize..4,
                crash_points in proptest::collection::vec(1u32..60, 0..4),
            ) {
                let (root, remote, files) = setup(n_parts, n_files);
                let payloads: Vec<Vec<u8>> = files
                    .iter()
                    .map(|f| std::fs::read(root.join(f)).expect("payload"))
                    .collect();
                drive(&root, &remote, &crash_points);
                let r = remote.borrow();
                prop_assert!(r.violations.is_empty(), "{:?}", r.violations);
                prop_assert!(r.partitions.is_empty());
                prop_assert!(r.rebuilds >= 1);
                for (f, want) in files.iter().zip(&payloads) {
                    prop_assert!(!root.join(f).exists());
                    let got = std::fs::read(root.join(held_target(f))).expect("held copy");
                    prop_assert_eq!(&got, want);
                }
                let done_valid = matches!(
                    tickvault_storage::held_days::read_day_marker(
                        &root.join(DAY_REPAIR_DIR),
                        MarkerKind::Done,
                        nov20()
                    ),
                    MarkerRead::Valid { .. }
    );
    prop_assert!(done_valid);
                drop(r);
                std::fs::remove_dir_all(&root).ok();
            }
        }
}
