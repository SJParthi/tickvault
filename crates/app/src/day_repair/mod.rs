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

pub mod coverage_run;
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

/// The progress object of the line written just before the rebuild starts
/// (2026-11-22): every later line belongs to a run that may hold rebuilt
/// rows.
pub const REBUILD_START_OBJECT: &str = "rebuild-start";

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
    /// Why the rebuild could not reproduce the day, found BEFORE anything is
    /// copied, moved or dropped (2026-11-21 review): a 20 Nov WAL segment on
    /// the box that is not in `raw-frames/`, no raw segment of the day, a
    /// second copy of a segment, the day's contract files unreadable, or a
    /// tick spill file whose name hour cannot tell 19, 20 and 21 Nov apart.
    pub raw_problems: Vec<String>,
}

/// The names of every WAL segment on the box whose name falls on `day`
/// (receipt day), in the WAL folder and its `replaying/`, `settling/` and
/// `archive/` folders, sorted and deduplicated. O(files).
#[must_use]
pub fn local_day_segment_names(wal_dir: &Path, day: NaiveDate) -> Vec<String> {
    let Ok(bounds) = rebuild::day_receipt_bounds_nanos(day) else {
        return Vec::new();
    };
    let mut out = std::collections::BTreeSet::new();
    for dir in [
        wal_dir.to_path_buf(),
        wal_dir.join("replaying"),
        wal_dir.join("settling"),
        wal_dir.join("archive"),
    ] {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(nanos) = tickvault_storage::ws_frame_spill::segment_name_nanos(name)
                .and_then(|n| i64::try_from(n).ok())
            else {
                continue;
            };
            if rebuild::receipt_day(nanos, bounds) == rebuild::ReceiptDay::On {
                out.insert(name.to_owned());
            }
        }
    }
    out.into_iter().collect()
}

/// The local segment names with no canonical copy among the raw-frame
/// listing's keys. Pure, O(local + keys).
#[must_use]
pub fn segments_missing_from_listing(local: &[String], keys: &[(String, u64)]) -> Vec<String> {
    let listed: std::collections::BTreeSet<&str> = keys
        .iter()
        .filter_map(|(k, _)| tickvault_storage::raw_frame_fetch::segment_name_of_key(k))
        .collect();
    local
        .iter()
        .filter(|n| !listed.contains(n.as_str()))
        .cloned()
        .collect()
}

/// How many hours after an IST midnight a tick spill file may still hold the
/// previous day's rows (the evening WAL drain runs until 05:00 IST; three
/// hours more of margin).
const SPILL_DAY_BOUNDARY_HOURS: u64 = 8;

/// Tick spill files whose name hour (the hour they were WRITTEN) falls in the
/// first [`SPILL_DAY_BOUNDARY_HOURS`] of `day` or of the day after: such a
/// file can hold the previous day's rows, so moving it (or leaving it) by its
/// name would put rows on the wrong side of the repair (2026-11-21 review).
/// `hours` is [`day_unix_hours`]. O(files).
#[must_use]
pub fn boundary_tick_spill_files(root: &Path, hours: (u64, u64)) -> Vec<PathBuf> {
    let next = hours.1.saturating_add(1);
    let in_boundary = |h: u64| {
        (h >= hours.0 && h < hours.0.saturating_add(SPILL_DAY_BOUNDARY_HOURS))
            || (h >= next && h < next.saturating_add(SPILL_DAY_BOUNDARY_HOURS))
    };
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root.join("data/spill/ticks")) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        if entry.file_type().is_ok_and(|t| t.is_file())
            && name
                .to_str()
                .and_then(tick_spill_hour)
                .is_some_and(in_boundary)
        {
            out.push(Path::new("data/spill/ticks").join(name));
        }
    }
    out.sort();
    out
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
    out.extend(pre.raw_problems.iter().cloned());
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
    /// Drops one partition in QuestDB (after its verified copy). Returns the
    /// quarantine key the drop verified against (the recorded copy, or a
    /// re-export with other bytes).
    async fn drop_partition(&mut self, p: &Partition, rec: &CopyRecord) -> anyhow::Result<String>;
    /// Every quarantine copy key of one partition (the canonical key and any
    /// `.sha-` re-export). Read only.
    async fn list_copy_keys(&mut self, table: &str, partition: &str)
    -> anyhow::Result<Vec<String>>;
    /// Copies one local day file (create-only) and verifies it.
    async fn copy_file(&mut self, rel: &Path) -> anyhow::Result<CopyRecord>;
    /// Rebuilds the day from its raw frames. `originals` are the quarantine
    /// keys holding the live day's tick-path rows ([`original_copy_keys`]).
    async fn rebuild(&mut self, originals: &[String]) -> anyhow::Result<RebuildReport>;
    /// The verify gate; returns the findings (empty = repaired).
    async fn verify(&mut self, rep: &RebuildReport) -> anyhow::Result<Vec<String>>;
    /// The packet coverage check: every dropped packet (by `capture_seq`) of
    /// the given quarantine copies must be in the rebuilt day; rows of the
    /// Quote 61 repeat-skip kinds are counted apart. `pinned` are the live
    /// day's copy keys per partition ([`pinned_original_keys`]; empty: every
    /// copy listed is read), `known` every tick-path partition copied at any
    /// time. Returns the findings.
    async fn coverage(
        &mut self,
        copies: &[(String, CopyRecord)],
        pinned: &std::collections::BTreeMap<String, Vec<String>>,
        known: &std::collections::BTreeSet<String>,
    ) -> anyhow::Result<Vec<String>>;
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
    // A run after a rebuild STARTED: record the live day's copies BEFORE this
    // run copies or drops anything, so no rebuilt row ever counts as live's.
    ensure_originals_pinned(io, log).await?;
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
    // The window is re-checked before every copy too (2026-11-21 review): a
    // long copy phase on a weekday morning must stop at 08:00 like the rest.
    for p in &pre.partitions {
        io.window_open().await?;
        ensure_copied(io, log, p).await?;
    }
    for rel in &files {
        io.window_open().await?;
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
        let used = io.drop_partition(p, &rec).await?;
        if used != rec.key {
            info!(object = %obj, first = %rec.key, second = %used, "day repair: dropped against a re-export");
        }
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
    // Marks that a rebuild began: from here on a drop's re-export holds
    // rebuilt rows, never the live day's.
    if !log.lines().iter().any(|l| l.step == Step::Rebuild) {
        note(log, Step::Rebuild, REBUILD_START_OBJECT, String::new())?;
    }
    let originals = live_copy_keys(io, log.lines()).await?;
    let rep = io.rebuild(&originals).await?;
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
    // The verify reads and exports for a long while; it runs only inside
    // the window like every other step (2026-11-21 review).
    io.window_open().await?;
    let mut findings = io.verify(&rep).await?;
    // Every dropped packet must be in the rebuilt day (2026-11-21: a row
    // count could not tell a missing packet from a re-routed replay row or a
    // Quote 61 repeat skip).
    // Presence only (a capture_seq in a rebuilt row): io.verify above already
    // checks the rebuilt rows' times and readings, so a packet present with a
    // wrong value is caught there, not here.
    let copies = dropped_tick_path_copies(log.lines());
    let pinned = pinned_original_keys(log.lines());
    let known = known_tick_path_objects(log.lines());
    findings.extend(coverage_list_findings(log.lines(), &copies));
    findings.extend(io.coverage(&copies, &pinned, &known).await?);
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

/// The tick-path tables whose rows conserve one row per packet across the
/// repair: each packet is a `ticks`, `tick_quotes` or `feed_aux_packets` row
/// before and after, only the table may change with the corrected time.
const TICK_PATH_TABLES: [&str; 3] = ["ticks", "tick_quotes", "feed_aux_packets"];

/// Whether `object` (`table/partition`) is a tick-path partition. Pure.
#[must_use]
pub fn is_tick_path_object(object: &str) -> bool {
    object
        .split_once('/')
        .is_some_and(|(table, _)| TICK_PATH_TABLES.contains(&table))
}

/// The index of the first `rebuild` line, or the line count when none:
/// every line before it belongs to the live day's copy and drop. Pure,
/// O(lines).
#[must_use]
pub fn first_rebuild_index(lines: &[ProgressLine]) -> usize {
    lines
        .iter()
        .position(|l| l.step == Step::Rebuild)
        .unwrap_or(lines.len())
}

/// The live day's copy keys per tick-path partition, from the `original`
/// lines (2026-11-22; one line per partition, `keys=<k1>,<k2>`). Empty when
/// none was recorded. Pure, O(lines).
#[must_use]
pub fn pinned_original_keys(
    lines: &[ProgressLine],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for l in lines.iter().filter(|l| l.step == Step::Original) {
        let Some(list) = l.detail.strip_prefix("keys=") else {
            continue;
        };
        let keys = out.entry(l.object.clone()).or_default();
        for key in list.split(',').filter(|k| !k.is_empty()) {
            if !keys.iter().any(|k| k == key) {
                keys.push(key.to_owned());
            }
        }
    }
    for keys in out.values_mut() {
        keys.sort();
    }
    out
}

/// Every pinned live-day copy key, sorted. Pure.
#[must_use]
pub fn original_copy_keys(lines: &[ProgressLine]) -> Vec<String> {
    let mut keys: Vec<String> = pinned_original_keys(lines)
        .into_values()
        .flatten()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Every tick-path partition copied at any time (the live day's and any the
/// rebuild created). Pure, O(lines).
#[must_use]
pub fn known_tick_path_objects(lines: &[ProgressLine]) -> std::collections::BTreeSet<String> {
    lines
        .iter()
        .filter(|l| l.step == Step::Copy && is_tick_path_object(&l.object))
        .map(|l| l.object.clone())
        .collect()
}

/// Whether a rebuild has started (a `rebuild` line of any object). Pure.
#[must_use]
pub fn rebuild_started(lines: &[ProgressLine]) -> bool {
    first_rebuild_index(lines) < lines.len()
}

/// The quarantine keys holding the live day's tick-path rows, for the
/// rebuild: the pinned keys when the live day is pinned, else (a first run,
/// before any rebuild, when every copy is the live day's) every copy listed
/// under each dropped partition. O(partitions) listings at most.
///
/// # Errors
/// A listing failed.
async fn live_copy_keys<I: RepairIo>(
    io: &mut I,
    lines: &[ProgressLine],
) -> anyhow::Result<Vec<String>> {
    let pinned = original_copy_keys(lines);
    if !pinned.is_empty() {
        return Ok(pinned);
    }
    let mut keys = Vec::new();
    for (obj, _) in dropped_tick_path_copies(lines) {
        let (table, partition) = obj
            .split_once('/')
            .with_context(|| format!("{obj} is not table/partition"))?;
        keys.extend(io.list_copy_keys(table, partition).await?);
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// Records the live day's copy keys of every dropped tick-path partition (one
/// `original` line per partition), once, at the start of any run after a
/// rebuild STARTED and before that run copies or drops anything: every key
/// under such a partition's quarantine prefix at that moment holds live rows
/// (a first run's copies and drop-time re-exports; nothing rebuilt has been
/// copied yet, since only a later drop re-exports rebuilt rows). A partition
/// already pinned is skipped, so a run that stopped half way finishes the pin
/// before it copies or drops. No-op before any rebuild (the first run needs
/// none). O(partitions) listings.
///
/// # Errors
/// A listing failed, or the recorded copy is not in the bucket.
async fn ensure_originals_pinned<I: RepairIo>(
    io: &mut I,
    log: &mut ProgressLog,
) -> anyhow::Result<()> {
    if !rebuild_started(log.lines()) {
        return Ok(());
    }
    let pinned = pinned_original_keys(log.lines());
    if pinned.is_empty() {
        // A run after the rebuild already copied or dropped a tick-path
        // partition without pinning (an older binary): a `.sha-` key may hold
        // rebuilt rows, and pinning it would call them live. Refuse.
        let after = &log.lines()[first_rebuild_index(log.lines())..];
        if let Some(l) = after
            .iter()
            .find(|l| matches!(l.step, Step::Copy | Step::Drop) && is_tick_path_object(&l.object))
        {
            bail!(
                "{} {} was recorded after the rebuild began, with no live-day pin; the \
                 quarantine may hold rebuilt rows under a live partition, refusing to pin",
                l.step.as_str(),
                l.object
            );
        }
    }
    for (obj, rec) in dropped_tick_path_copies(log.lines()) {
        if pinned.contains_key(&obj) {
            continue;
        }
        let (table, partition) = obj
            .split_once('/')
            .with_context(|| format!("{obj} is not table/partition"))?;
        let keys = io.list_copy_keys(table, partition).await?;
        if !keys.contains(&rec.key) {
            bail!(
                "the recorded copy {} of {obj} is not in the bucket; not pinning the live day",
                rec.key
            );
        }
        if let Some(bad) = keys.iter().find(|k| k.contains(',')) {
            bail!("a quarantine key holds a comma ({bad}); it cannot be pinned");
        }
        note(
            log,
            Step::Original,
            &obj,
            format!("keys={}", keys.join(",")),
        )?;
        io.audit(
            Step::Original,
            &obj,
            Some(&rec),
            "pinned",
            "pinned live copies",
        )
        .await?;
    }
    Ok(())
}

/// The dropped tick-path partitions' quarantine copies: the FIRST recorded
/// copy of each partition, recorded before the first rebuild (a re-run copies
/// its own rebuilt rows again under a later line, and a partition the rebuild
/// created is first copied after it; neither is the original day). O(lines).
#[must_use]
pub fn dropped_tick_path_copies(lines: &[ProgressLine]) -> Vec<(String, CopyRecord)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let live = &lines[..first_rebuild_index(lines)];
    for l in live.iter().filter(|l| l.step == Step::Copy) {
        let table = l.object.split('/').next().unwrap_or_default();
        if !TICK_PATH_TABLES.contains(&table) || !seen.insert(l.object.as_str()) {
            continue;
        }
        if let Some(rec) = CopyRecord::decode(&l.detail) {
            out.push((l.object.clone(), rec));
        }
    }
    out
}

/// What is wrong with the list of copies the coverage check reads: none at
/// all, or a dropped tick-path partition with no recorded copy (the check
/// would then pass while a dropped partition went unread). Pure, O(lines
/// log lines).
#[must_use]
pub fn coverage_list_findings(
    lines: &[ProgressLine],
    copies: &[(String, CopyRecord)],
) -> Vec<String> {
    if copies.is_empty() {
        return vec!["no tick-path quarantine copy to check coverage against".to_string()];
    }
    let have: std::collections::BTreeSet<&str> = copies.iter().map(|(o, _)| o.as_str()).collect();
    let mut out = Vec::new();
    // Drops after the first rebuild drop rebuilt rows, not the live day's.
    let live = &lines[..first_rebuild_index(lines)];
    for l in live.iter().filter(|l| l.step == Step::Drop) {
        let table = l.object.split('/').next().unwrap_or_default();
        if TICK_PATH_TABLES.contains(&table) && !have.contains(l.object.as_str()) {
            out.push(format!(
                "dropped {} has no recorded copy to check",
                l.object
            ));
        }
    }
    out.dedup();
    out
}

/// One `KEY=value` from the output of `systemctl show <unit> -p Environment
/// --value`: space-separated assignments, an assignment holding a space
/// quoted whole (`"KEY=a b"`). `None` when the key is absent or empty. Pure,
/// O(text).
#[must_use]
pub fn unit_environment_value(text: &str, key: &str) -> Option<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in text.trim().chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .into_iter()
        .filter_map(|w| w.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .filter(|(k, v)| k == key && !v.trim().is_empty())
        .map(|(_, v)| v.trim().to_owned())
        .next_back()
}

/// The environment name the repair runs under: this process's own
/// (`TV_ENVIRONMENT` / `ENVIRONMENT`, already checked by the storage
/// crate), else the app unit's. Only `[a-zA-Z0-9-]` is accepted (it names a
/// bucket and an SSM path). Pure.
#[must_use]
pub fn repair_environment(process: Option<String>, unit: Option<String>) -> Option<String> {
    process
        .or(unit)
        .filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
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
/// The keep-alive instant to write: now plus `hours`, never earlier than a
/// later value already set (the runner's, or an earlier step's) when `hours`
/// is positive (2026-11-21 review: a fixed Monday cap wrote a time already
/// past and let the curfew stop the box mid-repair). `hours <= 0` releases:
/// exactly now. `current` is `YYYY-MM-DDTHH:MM:SS`, any suffix ignored.
/// Pure.
#[must_use]
pub fn keep_alive_target(now: NaiveDateTime, hours: i64, current: Option<&str>) -> NaiveDateTime {
    let wanted = now + chrono::Duration::hours(hours.max(0));
    if hours <= 0 {
        return now;
    }
    current
        .and_then(|c| c.get(..19))
        .and_then(|c| NaiveDateTime::parse_from_str(c, "%Y-%m-%dT%H:%M:%S").ok())
        .map_or(wanted, |c| c.max(wanted))
}

async fn write_keep_alive(env: &str, hours: i64) {
    let ssm = tickvault_core::auth::secret_manager::create_ssm_client_public().await;
    let name = format!("/tickvault/{env}/keep-alive-until");
    let current = if hours > 0 {
        tokio::time::timeout(
            KEEP_ALIVE_PUT_TIMEOUT,
            ssm.get_parameter().name(&name).send(),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|r| r.parameter().and_then(|p| p.value()).map(str::to_owned))
    } else {
        None
    };
    let until = keep_alive_target(now_ist(), hours, current.as_deref());
    let value = until.format("%Y-%m-%dT%H:%M:%S").to_string();
    let put = ssm
        .put_parameter()
        .name(format!("/tickvault/{env}/keep-alive-until"))
        .value(&value)
        .r#type(aws_sdk_ssm::types::ParameterType::String)
        .overwrite(true)
        .send();
    match tokio::time::timeout(KEEP_ALIVE_PUT_TIMEOUT, put).await {
        Ok(Ok(_)) => say(&format!("keep-alive-until set to {value} IST")),
        // An error, not a warning: without the keep-alive the hourly curfew
        // can stop the box in the middle of the repair.
        Ok(Err(e)) => error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_keep_alive",
            error = %aws_sdk_ssm::error::DisplayErrorContext(&e),
            "day repair: could not write keep-alive-until"
        ),
        Err(_) => error!(
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
    // The SSM shell runs as root with none of the app unit's environment
    // (2026-11-21, first dry run: "no cold bucket resolves"), so the
    // environment name is read from the unit itself when this process has
    // none, and the cold bucket is named from it when the config has none.
    let (_, unit_env_text) = systemctl(&["show", APP_UNIT, "-p", "Environment", "--value"]);
    let env = repair_environment(
        tickvault_storage::s3_cold::runtime_environment(),
        unit_environment_value(&unit_env_text, "TV_ENVIRONMENT"),
    )
    .context("no environment name: TV_ENVIRONMENT is set neither here nor on the app unit")?;
    let mut config: tickvault_common::config::ApplicationConfig =
        crate::boot_helpers::layered_config_figment(&env)
            .extract()
            .context("load the configuration")?;
    if config.partition_retention.archive_bucket.trim().is_empty() {
        config.partition_retention.archive_bucket = format!("tv-{env}-cold");
    }
    say(&format!(
        "environment {env}, cold bucket {}",
        config.partition_retention.archive_bucket
    ));
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
    // Apply AND verify hold and stop the app (2026-11-21 review): the app
    // unit starts at every boot, so a box the runner started has it running.
    if let Err(e) = hold_and_stop_app() {
        finish_box(&env).await;
        return Err(e);
    }
    let wal_guard = match tickvault_storage::ws_frame_spill::lock_wal_dir(&wal_dir) {
        Ok(Some(g)) => g,
        Ok(None) => {
            say("REFUSED: the WAL folder lock could not be taken (unwritable folder)");
            finish_box(&env).await;
            return Ok(EXIT_REFUSED);
        }
        Err(e) => {
            finish_box(&env).await;
            return Err(e.context("another process owns the WAL folder"));
        }
    };
    write_keep_alive(&env, if mode == Mode::Apply { 8 } else { 3 }).await;
    let progress_dir = root.join(DAY_REPAIR_DIR);
    let mut log = match ProgressLog::open(&ProgressLog::path_for(&progress_dir, REPAIR_DAY)) {
        Ok(l) => l,
        Err(e) => {
            drop(wal_guard);
            finish_box(&env).await;
            return Err(anyhow::anyhow!("{e}"));
        }
    };
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
    // Every way out puts the box back to normal (2026-11-21 review): the WAL
    // folder released, the unit hold removed and the app STARTED, so a box
    // left running (or reaching 08:00 on a session day) never sits with the
    // app down. The held day stays held until `.done`; files are already
    // aside before any drop, so the app replays nothing of it.
    drop(wal_guard);
    finish_box(&env).await;
    if let Err(ref e) = result
        && format!("{e:#}").contains(WINDOW_STOP)
    {
        say(&format!(
            "STOPPED: {e:#}; the app is running again, the day stays held, re-run in the next \
             window to resume"
        ));
        return Ok(EXIT_REFUSED);
    }
    result?;
    if mode == Mode::Verify {
        say("verify passed; .done written, the app is running again");
    }
    Ok(EXIT_OK)
}

/// Removes the unit hold, starts the app and clears the repair's keep-alive.
/// Cold, once per run.
async fn finish_box(env: &str) {
    release_app_unit();
    let (started, out) = systemctl(&["start", APP_UNIT]);
    if started {
        say("the app unit is started again");
    } else {
        error!(
            code = ErrorCode::DhanLtt02RecordUnsettled.code_str(),
            source = "day_repair_app_restart",
            output = %out,
            "day repair: could not start the app again; start it by hand"
        );
    }
    write_keep_alive(env, 0).await;
}

/// `dry`: every guard and object, printed; nothing changes. Exit 3 when
/// `apply` would refuse.
async fn dry_report<I: RepairIo>(
    io: &mut I,
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

    impl ProdIo {
        /// Everything the rebuild needs, checked before any copy, move or
        /// drop (2026-11-21 review): the day's contract files, a raw segment
        /// of the day and no second copy of one, every 20 Nov WAL segment on
        /// the box present in `raw-frames/`, and no tick spill file whose
        /// name hour cannot tell the days apart. A check that cannot run is a
        /// problem too (refuse, never guess). Cold, once per run.
        async fn raw_source_problems(&self) -> Vec<String> {
            let mut out = Vec::new();
            let day_text = self.day.format("%Y-%m-%d").to_string();
            if let Err(e) = crate::dhan_contract_universe::contract_names_for_day_strict(&day_text)
            {
                out.push(format!("the day's contract names cannot be built: {e:#}"));
            }
            match tickvault_storage::s3_cold::S3Cold::load(
                &self.config.partition_retention.archive_bucket,
            )
            .await
            {
                None => out.push("no cold bucket resolves (TV_ENVIRONMENT)".to_owned()),
                Some(s3) => {
                    match tickvault_storage::raw_frame_fetch::list_raw_frame_day(&s3, &day_text)
                        .await
                    {
                        Err(e) => out.push(format!("the raw-frame listing failed: {e:#}")),
                        Ok(listing) => {
                            if listing.day_segments == 0 {
                                out.push(format!("no raw segment under raw-frames/{day_text}/"));
                            }
                            if !listing.sidecars.is_empty() {
                                out.push(format!(
                                    "a second, different copy of a raw segment exists: {}",
                                    listing.sidecars.join(", ")
                                ));
                            }
                            let wal_dir = crate::boot_helpers::ws_wal_dir();
                            let local = super::local_day_segment_names(&wal_dir, self.day);
                            let missing =
                                super::segments_missing_from_listing(&local, &listing.keys);
                            if !missing.is_empty() {
                                out.push(format!(
                                "{} WAL segment(s) of {day_text} on the box are not in raw-frames/ \
                                 (the rebuild would leave their rows out): {}",
                                missing.len(),
                                missing.join(", ")
                            ));
                            }
                        }
                    }
                }
            }
            match super::day_unix_hours(self.day) {
                Ok(hours) => {
                    let boundary = super::boundary_tick_spill_files(&self.root, hours);
                    if !boundary.is_empty() {
                        out.push(format!(
                            "tick spill file(s) written near a day boundary may hold another \
                             day's rows; drain or check them first: {}",
                            boundary
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
                Err(e) => out.push(format!("the day's hours: {e:#}")),
            }
            out
        }
    }

    /// Lowercase hex to bytes; `None` on an odd length or a bad digit. Pure.
    pub(super) fn hex_decode(hex: &str) -> Option<Vec<u8>> {
        if !hex.len().is_multiple_of(2) {
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
            // The live-day pin is a listing record (2026-11-22): an
            // `enumerate` row whose detail says `pinned live copies`.
            Step::Enumerate | Step::Original => DayRepairStep::Enumerate,
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
            let raw_problems = self.raw_source_problems().await;
            Ok(Preflight {
                partitions,
                canonical_objects,
                brutex_done,
                fingerprints,
                raw_problems,
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

        async fn drop_partition(
            &mut self,
            p: &Partition,
            rec: &CopyRecord,
        ) -> anyhow::Result<String> {
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
                    Ok(c.s3_key)
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

        async fn list_copy_keys(
            &mut self,
            table: &str,
            partition: &str,
        ) -> anyhow::Result<Vec<String>> {
            use tickvault_storage::day_repair_coverage as cov;
            let s3 = tickvault_storage::s3_cold::S3Cold::load(
                &self.config.partition_retention.archive_bucket,
            )
            .await
            .context("no cold bucket resolves")?;
            let prefix = cov::partition_copy_prefix(table, partition);
            Ok(cov::copies_under_prefix(
                &cov::list_copy_keys(&s3, &prefix).await?,
                &prefix,
            ))
        }

        async fn rebuild(&mut self, originals: &[String]) -> anyhow::Result<RebuildReport> {
            rebuild::rebuild_day(&self.config, self.day, &self.root, &self.staging, originals).await
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

        async fn coverage(
            &mut self,
            copies: &[(String, CopyRecord)],
            pinned: &std::collections::BTreeMap<String, Vec<String>>,
            known: &std::collections::BTreeSet<String>,
        ) -> anyhow::Result<Vec<String>> {
            let s3 = tickvault_storage::s3_cold::S3Cold::load(
                &self.config.partition_retention.archive_bucket,
            )
            .await
            .context("no cold bucket resolves")?;
            let exp_url = self.exec_url.replacen("/exec", "/exp", 1);
            // An export of tens of millions of rows outlives the probe client.
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(1_800))
                .build()
                .context("build the export client")?;
            let d = self.day.format("%Y-%m-%d").to_string();
            let config = &self.config;
            super::coverage_run::run_coverage(
                &super::coverage_run::S3Store(s3),
                &client,
                &exp_url,
                &d,
                &self.staging,
                copies,
                super::coverage_run::CoverageScope { pinned, known },
                || super::window_open_now(config),
            )
            .await
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
    /// The adapter's pure helpers (2026-11-21).
    #[cfg(test)]
    mod helper_tests {
        use super::*;

        #[test]
        fn test_hex_decode_and_base64_match_known_vectors() {
            assert_eq!(hex_decode("00ff10"), Some(vec![0, 255, 16]));
            assert_eq!(hex_decode(""), Some(Vec::new()));
            assert_eq!(hex_decode("abc"), None);
            assert_eq!(hex_decode("zz"), None);
            // RFC 4648 test vectors.
            for (raw, b64) in [
                ("", ""),
                ("f", "Zg=="),
                ("fo", "Zm8="),
                ("foo", "Zm9v"),
                ("foob", "Zm9vYg=="),
                ("fooba", "Zm9vYmE="),
                ("foobar", "Zm9vYmFy"),
            ] {
                assert_eq!(base64(raw.as_bytes()), b64, "{raw}");
            }
            // The SHA-256 of nothing, as S3 writes it.
            let sha =
                hex_decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                    .expect("hex");
            assert_eq!(base64(&sha), "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
        }

        #[test]
        fn test_record_outcome_and_step_mappings() {
            let obj = QuarantinedObject {
                object: "ticks/2026-11-20T09".to_owned(),
                s3_key: "quarantine/x".to_owned(),
                rows: None,
                bytes: 12,
                sha256_hex: "ab".to_owned(),
                reused: false,
            };
            let rec = to_record(&obj);
            assert_eq!(
                (rec.key.as_str(), rec.rows, rec.bytes),
                ("quarantine/x", 0, 12)
            );
            for (o, want) in [
                (QuarantineOutcome::Refused { reason: "r" }, "refused (r)"),
                (
                    QuarantineOutcome::Kept {
                        stage: "s",
                        detail: "d".to_owned(),
                    },
                    "kept at s: d",
                ),
                (QuarantineOutcome::Copied(obj.clone()), "not dropped"),
                (
                    QuarantineOutcome::Dropped(obj.clone()),
                    "dropped unexpectedly",
                ),
            ] {
                let e = format!("{:#}", outcome_error("p", o));
                assert!(e.starts_with("p: ") && e.contains(want), "{e}");
            }
            let steps = [
                Step::Enumerate,
                Step::Copy,
                Step::Drop,
                Step::Move,
                Step::Rebuild,
                Step::Verify,
                Step::Done,
            ];
            let mapped: std::collections::BTreeSet<String> = steps
                .iter()
                .map(|s| format!("{:?}", audit_step(*s)))
                .collect();
            assert_eq!(
                mapped.len(),
                steps.len(),
                "each step maps to its own audit step"
            );
            assert_eq!(audit_outcome("failed"), DayRepairOutcome::Failed);
            assert_eq!(audit_outcome("unfinished"), DayRepairOutcome::Failed);
            assert_eq!(audit_outcome("copied"), DayRepairOutcome::Copied);
            assert_eq!(audit_outcome("dropped"), DayRepairOutcome::Dropped);
            assert_eq!(audit_outcome("listed"), DayRepairOutcome::Ok);
        }
    }
}

#[cfg(test)]
mod dropped_rows_tests {
    use super::*;

    /// 2026-11-21 first dry run: the SSM shell had no TV_ENVIRONMENT, so the
    /// name comes from the app unit, read the way systemd prints it.
    #[test]
    fn test_unit_environment_value_and_repair_environment() {
        let text = "RUST_LOG=info TV_ENVIRONMENT=prod \"NOTE=a b\" AWS_CONFIG_FILE=/dev/null\n";
        assert_eq!(
            unit_environment_value(text, "TV_ENVIRONMENT").as_deref(),
            Some("prod")
        );
        assert_eq!(unit_environment_value(text, "NOTE").as_deref(), Some("a b"));
        assert_eq!(unit_environment_value(text, "MISSING"), None);
        assert_eq!(
            unit_environment_value("TV_ENVIRONMENT=", "TV_ENVIRONMENT"),
            None
        );
        assert_eq!(unit_environment_value("", "TV_ENVIRONMENT"), None);
        // A later assignment wins, as in systemd.
        assert_eq!(
            unit_environment_value("TV_ENVIRONMENT=a TV_ENVIRONMENT=b", "TV_ENVIRONMENT")
                .as_deref(),
            Some("b")
        );
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(repair_environment(s("dev"), s("prod")), s("dev"));
        assert_eq!(repair_environment(None, s("prod")), s("prod"));
        assert_eq!(repair_environment(None, None), None);
        assert_eq!(repair_environment(None, s("../etc")), None);
        assert_eq!(repair_environment(s(""), s("prod")), None);
    }

    #[test]
    fn test_keep_alive_target_never_shortens_and_releases_to_now() {
        let now =
            NaiveDateTime::parse_from_str("2026-11-21T20:00:00", "%Y-%m-%dT%H:%M:%S").expect("now");
        let at = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").expect("t");
        assert_eq!(keep_alive_target(now, 8, None), at("2026-11-22T04:00:00"));
        // A later value (the runner's, with its offset suffix) is kept.
        assert_eq!(
            keep_alive_target(now, 3, Some("2026-11-22T05:00:00+05:30")),
            at("2026-11-22T05:00:00")
        );
        // An earlier or unreadable value never shortens the wanted one.
        assert_eq!(
            keep_alive_target(now, 8, Some("2026-11-21T07:30:00")),
            at("2026-11-22T04:00:00")
        );
        assert_eq!(
            keep_alive_target(now, 8, Some("garbage")),
            at("2026-11-22T04:00:00")
        );
        // A Monday run is not capped at a fixed past instant any more.
        let monday = at("2026-11-23T20:00:00");
        assert_eq!(
            keep_alive_target(monday, 8, None),
            at("2026-11-24T04:00:00")
        );
        // Release: exactly now, whatever was set.
        assert_eq!(keep_alive_target(now, 0, Some("2026-11-30T00:00:00")), now);
    }

    fn copy(object: &str, rows: u64) -> ProgressLine {
        ProgressLine {
            step: Step::Copy,
            object: object.to_owned(),
            detail: CopyRecord {
                key: format!("quarantine/{object}"),
                sha256: "ab".repeat(32),
                rows,
                bytes: rows * 10,
            }
            .encode(),
        }
    }

    #[test]
    fn test_dropped_tick_path_copies_take_the_first_copy_of_tick_path_tables_only() {
        let lines = vec![
            copy("ticks/2026-11-20T09", 100),
            copy("tick_quotes/2026-11-20T09", 40),
            copy("feed_aux_packets/2026-11-20", 7),
            copy("candles_1m/2026-11-20", 999),
            // A re-run's copy of its own rebuilt rows: not the original day.
            copy("ticks/2026-11-20T09", 500),
        ];
        let got = dropped_tick_path_copies(&lines);
        let objects: Vec<&str> = got.iter().map(|(o, _)| o.as_str()).collect();
        assert_eq!(
            objects,
            vec![
                "ticks/2026-11-20T09",
                "tick_quotes/2026-11-20T09",
                "feed_aux_packets/2026-11-20"
            ]
        );
        assert_eq!(got[0].1.rows, 100, "the first copy, not the re-run's");
        assert!(dropped_tick_path_copies(&[]).is_empty());
    }

    #[test]
    fn test_coverage_list_findings_refuse_an_empty_list_and_an_uncopied_drop() {
        let mut drop_line = copy("ticks/2026-11-20T10", 5);
        drop_line.step = Step::Drop;
        let mut candle_drop = copy("candles_1m/2026-11-20", 5);
        candle_drop.step = Step::Drop;
        let lines = vec![copy("ticks/2026-11-20T09", 100), drop_line, candle_drop];
        let copies = dropped_tick_path_copies(&lines);
        let got = coverage_list_findings(&lines, &copies);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("ticks/2026-11-20T10"));
        assert_eq!(coverage_list_findings(&lines, &[]).len(), 1);
        let ok = vec![copy("ticks/2026-11-20T09", 100)];
        let copies = dropped_tick_path_copies(&ok);
        assert!(coverage_list_findings(&ok, &copies).is_empty());
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
            raw_problems: Vec::new(),
        };
        assert!(apply_refusals(&clean, &[], REPAIR_DAY).is_empty());
        let raw = Preflight {
            raw_problems: vec!["a".to_owned(), "b".to_owned()],
            ..clean.clone()
        };
        assert_eq!(apply_refusals(&raw, &[], REPAIR_DAY).len(), 2);
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

    /// 2026-11-21 review: a 20 Nov segment the boot replay already moved to
    /// `settling/` or `archive/` must still be found, so a segment missing from
    /// `raw-frames/` refuses the apply before any drop.
    #[test]
    fn test_local_day_segment_names_and_segments_missing_from_listing_cover_every_wal_folder() {
        let root = scratch("walall");
        let wal = root.join("ws_wal");
        for d in ["replaying", "settling", "archive"] {
            std::fs::create_dir_all(wal.join(d)).expect("dir");
        }
        let (start, end) = rebuild::day_receipt_bounds_nanos(nov20()).expect("bounds");
        let seg = |n: i64| format!("ws-frames-{n:020}.wal");
        let a = seg(start + 1);
        let b = seg(start + 3_600_000_000_000);
        let c = seg(end - 1);
        std::fs::write(wal.join(&a), b"").expect("seg");
        std::fs::write(wal.join("settling").join(&b), b"").expect("seg");
        std::fs::write(wal.join("archive").join(&c), b"").expect("seg");
        std::fs::write(wal.join("archive").join(seg(end)), b"").expect("next day");
        std::fs::write(wal.join("archive").join(seg(start - 1)), b"").expect("day before");
        let local = local_day_segment_names(&wal, nov20());
        assert_eq!(local, vec![a.clone(), b.clone(), c.clone()]);
        let keys = vec![
            (format!("raw-frames/2026-11-20/{a}.gz"), 10),
            (format!("raw-frames/2026-11-20/{c}.gz"), 10),
        ];
        assert_eq!(segments_missing_from_listing(&local, &keys), vec![b]);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 2026-11-21 review: a tick spill file written in the first hours of a
    /// day may hold the day before's rows, so it refuses the apply.
    #[test]
    fn test_boundary_tick_spill_files_flag_only_the_ambiguous_hours() {
        let root = scratch("spillb");
        let dir = root.join("data/spill/ticks");
        std::fs::create_dir_all(&dir).expect("dir");
        let hours = day_unix_hours(nov20()).expect("hours");
        let name = |h: u64| format!("ticks-dhan-{h}.ilp");
        for h in [
            hours.0,
            hours.0 + 7,
            hours.0 + 8,
            hours.1,
            hours.1 + 1,
            hours.1 + 9,
        ] {
            std::fs::write(dir.join(name(h)), b"x").expect("file");
        }
        let got = boundary_tick_spill_files(&root, hours);
        let want: Vec<PathBuf> = {
            let mut v: Vec<PathBuf> = [hours.0, hours.0 + 7, hours.1 + 1]
                .iter()
                .map(|h| PathBuf::from("data/spill/ticks").join(name(*h)))
                .collect();
            v.sort();
            v
        };
        assert_eq!(got, want);
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
        /// The rebuild writes rows again into the live tick partitions and
        /// creates one more (a later drop then re-exports REBUILT rows).
        rebuild_recreates: bool,
        /// Verifies still to fail (each a finding).
        fail_verifies: u32,
        /// The `originals` each rebuild was handed.
        originals_seen: Vec<Vec<String>>,
        /// `(copies objects, pinned)` each coverage call was handed.
        coverage_seen: Vec<(Vec<String>, BTreeMap<String, Vec<String>>)>,
        /// The tick partitions of the live day (what a rebuild recreates).
        live_tick_partitions: Vec<Partition>,
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
        async fn drop_partition(
            &mut self,
            p: &Partition,
            rec: &CopyRecord,
        ) -> anyhow::Result<String> {
            self.tick()?;
            let mut r = self.remote.borrow_mut();
            if r.copies.get(&format!("q/{}", p.object())) != Some(rec) {
                r.violations
                    .push(format!("drop of {} without its copy", p.object()));
            }
            // After a rebuild the partition holds rebuilt rows: the drop's
            // re-export lands at a `.sha-` key beside the live copy.
            let used = if r.rebuilds > 0 {
                let moved = CopyRecord {
                    key: format!("q/{}.sha-bb", p.object()),
                    sha256: "bb".to_owned(),
                    rows: 1,
                    bytes: 1,
                };
                r.copies.insert(moved.key.clone(), moved.clone());
                moved.key
            } else {
                rec.key.clone()
            };
            r.partitions.remove(p);
            drop(r);
            self.tick()?;
            Ok(used)
        }
        async fn list_copy_keys(
            &mut self,
            table: &str,
            partition: &str,
        ) -> anyhow::Result<Vec<String>> {
            self.tick()?;
            let base = format!("q/{table}/{partition}");
            Ok(self
                .remote
                .borrow()
                .copies
                .keys()
                .filter(|k| **k == base || k.starts_with(&format!("{base}.sha-")))
                .cloned()
                .collect())
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
        async fn rebuild(&mut self, originals: &[String]) -> anyhow::Result<RebuildReport> {
            self.tick()?;
            let hours = day_unix_hours(nov20())?;
            let mut r = self.remote.borrow_mut();
            r.originals_seen.push(originals.to_vec());
            if !r.partitions.is_empty() {
                r.violations
                    .push("rebuild before every partition was dropped".to_owned());
            }
            if !list_day_files(&self.root, REPAIR_DAY, hours).is_empty() {
                r.violations
                    .push("rebuild before every file was moved".to_owned());
            }
            r.rebuilds += 1;
            if r.rebuild_recreates {
                let mut parts = r.live_tick_partitions.clone();
                parts.push(Partition {
                    table: "tick_quotes".to_owned(),
                    partition: format!("{REPAIR_DAY}T15"),
                });
                r.partitions.extend(parts);
            }
            drop(r);
            self.tick()?;
            // More rows than any copy holds, so the dropped-rows check passes.
            Ok(RebuildReport {
                frames: 1,
                refolded: 1,
                tick_rows: 1_000,
                ..RebuildReport::default()
            })
        }
        async fn window_open(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn verify(&mut self, _rep: &RebuildReport) -> anyhow::Result<Vec<String>> {
            self.tick()?;
            let mut r = self.remote.borrow_mut();
            r.verifies += 1;
            if r.fail_verifies > 0 {
                r.fail_verifies -= 1;
                return Ok(vec!["injected verify finding".to_owned()]);
            }
            Ok(Vec::new())
        }
        async fn coverage(
            &mut self,
            copies: &[(String, CopyRecord)],
            pinned: &BTreeMap<String, Vec<String>>,
            _known: &BTreeSet<String>,
        ) -> anyhow::Result<Vec<String>> {
            self.tick()?;
            self.remote.borrow_mut().coverage_seen.push((
                copies.iter().map(|(o, _)| o.clone()).collect(),
                pinned.clone(),
            ));
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
        remote.live_tick_partitions = remote
            .partitions
            .iter()
            .filter(|p| p.table == "ticks")
            .cloned()
            .collect();
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

    /// `dry` lists and judges and changes nothing: no copy, drop or move,
    /// and a waiting 20 Nov WAL segment turns the verdict into a refusal.
    #[test]
    fn test_dry_report_lists_judges_and_changes_nothing() {
        let (root, remote, files) = setup(3, 2);
        let wal = root.join("data/ws_wal");
        let mut io = FakeIo {
            remote: &remote,
            root: root.clone(),
            fail_at: None,
            calls: 0,
        };
        let code = block_on(dry_report(&mut io, &root, &wal, nov20())).expect("dry");
        assert_eq!(code, EXIT_OK);
        let r = remote.borrow();
        assert!(r.copies.is_empty(), "dry copied nothing");
        assert_eq!(r.partitions.len(), 3, "dry dropped nothing");
        assert_eq!(r.rebuilds, 0);
        drop(r);
        for f in &files {
            assert!(root.join(f).exists(), "dry moved nothing: {}", f.display());
        }
        std::fs::create_dir_all(&wal).expect("wal dir");
        let (start, _) = rebuild::day_receipt_bounds_nanos(nov20()).expect("bounds");
        std::fs::write(wal.join(format!("ws-frames-{:020}.wal", start + 1)), b"").expect("seg");
        let code = block_on(dry_report(&mut io, &root, &wal, nov20())).expect("dry");
        assert_eq!(code, EXIT_REFUSED);
        std::fs::remove_dir_all(&root).ok();
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

    /// The 2026-11-20 shape: apply, a verify that fails, a second apply over
    /// the rebuilt partitions (whose drops re-export REBUILT rows and copy a
    /// partition the rebuild created), then a verify that passes. The live
    /// day's keys are pinned before the second run copies or drops anything,
    /// so the second rebuild and the coverage read the live copies only.
    #[test]
    fn test_rerun_after_a_rebuild_reads_only_the_live_day() {
        let (root, remote, _files) = setup(3, 0);
        {
            let mut r = remote.borrow_mut();
            r.rebuild_recreates = true;
            r.fail_verifies = 1;
        }
        assert_eq!(drive(&root, &remote, &[]), 2);
        let r = remote.borrow();
        assert!(r.violations.is_empty(), "{:?}", r.violations);
        let live = vec![
            format!("q/ticks/{REPAIR_DAY}T09"),
            format!("q/ticks/{REPAIR_DAY}T10"),
        ];
        assert_eq!(r.originals_seen, vec![live.clone(), live.clone()]);
        // The second run's drops did re-export rebuilt rows.
        assert!(
            r.copies.keys().any(|k| k.contains(".sha-")),
            "{:?}",
            r.copies
        );
        for (objects, pinned) in &r.coverage_seen {
            assert_eq!(
                objects,
                &vec![
                    format!("ticks/{REPAIR_DAY}T09"),
                    format!("ticks/{REPAIR_DAY}T10")
                ]
            );
            let keys: Vec<String> = pinned.values().flatten().cloned().collect();
            assert!(
                keys.iter()
                    .all(|k| !k.contains(".sha-") && !k.contains("tick_quotes"))
            );
        }
        // The second coverage ran pinned, on exactly the live keys.
        let (_, last) = r.coverage_seen.last().expect("coverage ran");
        assert_eq!(last.values().flatten().cloned().collect::<Vec<_>>(), live);
        std::fs::remove_dir_all(&root).ok();
    }

    fn pline(step: Step, object: &str, detail: &str) -> ProgressLine {
        ProgressLine {
            step,
            object: object.to_owned(),
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn test_first_rebuild_index_and_rebuild_started_see_the_start_line() {
        let mut lines = vec![pline(Step::Copy, "ticks/A", "")];
        assert_eq!(first_rebuild_index(&lines), 1);
        assert!(!rebuild_started(&lines));
        lines.push(pline(Step::Rebuild, REBUILD_START_OBJECT, ""));
        lines.push(pline(Step::Rebuild, DAY_OBJECT, ""));
        assert_eq!(first_rebuild_index(&lines), 1);
        assert!(rebuild_started(&lines));
    }

    #[test]
    fn test_is_tick_path_object_names_the_three_tables_only() {
        for o in ["ticks/x", "tick_quotes/x", "feed_aux_packets/x"] {
            assert!(is_tick_path_object(o), "{o}");
        }
        for o in [
            "candles_1m/x",
            "market_depth/x",
            "ticks",
            "data/spill/ticks/a",
        ] {
            assert!(!is_tick_path_object(o), "{o}");
        }
    }

    #[test]
    fn test_pinned_original_keys_and_original_copy_keys_merge_lines() {
        let lines = vec![
            pline(Step::Original, "ticks/A", "keys=q/b,q/a"),
            pline(Step::Original, "ticks/A", "keys=q/a,,q/c"),
            pline(Step::Original, "ticks/B", "keys="),
            pline(Step::Copy, "ticks/C", "keys=q/x"),
        ];
        let pinned = pinned_original_keys(&lines);
        assert_eq!(pinned["ticks/A"], vec!["q/a", "q/b", "q/c"]);
        assert!(pinned.get("ticks/B").is_some_and(Vec::is_empty));
        assert!(!pinned.contains_key("ticks/C"));
        assert_eq!(original_copy_keys(&lines), vec!["q/a", "q/b", "q/c"]);
    }

    #[test]
    fn test_known_tick_path_objects_lists_every_copied_tick_partition() {
        let lines = vec![
            pline(Step::Copy, "ticks/A", ""),
            pline(Step::Copy, "candles_5m/A", ""),
            pline(Step::Drop, "tick_quotes/B", ""),
            pline(Step::Copy, "tick_quotes/C", ""),
        ];
        let known: Vec<String> = known_tick_path_objects(&lines).into_iter().collect();
        assert_eq!(known, vec!["tick_quotes/C", "ticks/A"]);
    }

    #[test]
    fn test_ensure_originals_pinned_refuses_a_run_that_copied_after_the_rebuild() {
        let (root, remote, _files) = setup(3, 0);
        let mut io = FakeIo {
            remote: &remote,
            root: root.clone(),
            fail_at: None,
            calls: 0,
        };
        let progress = ProgressLog::path_for(&root.join(DAY_REPAIR_DIR), REPAIR_DAY);
        let mut log = ProgressLog::open(&progress).expect("log");
        let rec = CopyRecord {
            key: format!("q/ticks/{REPAIR_DAY}T09"),
            sha256: "aa".to_owned(),
            rows: 1,
            bytes: 1,
        };
        for l in [
            pline(Step::Copy, &format!("ticks/{REPAIR_DAY}T09"), &rec.encode()),
            pline(Step::Rebuild, DAY_OBJECT, ""),
            pline(Step::Copy, &format!("ticks/{REPAIR_DAY}T09"), &rec.encode()),
        ] {
            log.append(l).expect("append");
        }
        let err = block_on(ensure_originals_pinned(&mut io, &mut log)).expect_err("refused");
        assert!(format!("{err:#}").contains("refusing to pin"), "{err:#}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_pin_helpers_parse_merge_and_bound_the_live_era() {
        let l = |step, object: &str, detail: &str| ProgressLine {
            step,
            object: object.to_owned(),
            detail: detail.to_owned(),
        };
        let rec = |k: &str| CopyRecord {
            key: k.to_owned(),
            sha256: "aa".to_owned(),
            rows: 1,
            bytes: 1,
        };
        let lines = vec![
            l(Step::Copy, "ticks/A", &rec("q/a").encode()),
            l(Step::Copy, "candles_1m/A", &rec("q/c").encode()),
            l(Step::Drop, "ticks/A", &rec("q/a").encode()),
            l(Step::Rebuild, DAY_OBJECT, ""),
            l(Step::Original, "ticks/A", "keys=q/a,q/a.sha-1"),
            l(Step::Original, "ticks/A", "keys=q/a"),
            l(Step::Original, "ticks/B", "nonsense"),
            l(Step::Copy, "tick_quotes/T15", &rec("q/t").encode()),
            l(Step::Drop, "tick_quotes/T15", &rec("q/t").encode()),
        ];
        assert_eq!(first_rebuild_index(&lines), 3);
        assert!(rebuild_started(&lines));
        assert!(!rebuild_started(&lines[..3]));
        let pinned = pinned_original_keys(&lines);
        assert_eq!(pinned.len(), 1);
        assert_eq!(
            pinned["ticks/A"],
            vec!["q/a".to_owned(), "q/a.sha-1".to_owned()]
        );
        assert_eq!(original_copy_keys(&lines), vec!["q/a", "q/a.sha-1"]);
        let copies = dropped_tick_path_copies(&lines);
        assert_eq!(
            copies.len(),
            1,
            "the rebuild-era partition is not live: {copies:?}"
        );
        assert_eq!(copies[0].0, "ticks/A");
        assert!(coverage_list_findings(&lines, &copies).is_empty());
        let known = known_tick_path_objects(&lines);
        assert!(known.contains("ticks/A") && known.contains("tick_quotes/T15"));
        assert!(!known.contains("candles_1m/A"));
        assert!(is_tick_path_object("feed_aux_packets/x"));
        assert!(!is_tick_path_object("candles_1m/x"));
        assert!(!is_tick_path_object("ticks"));
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
                n_parts in 2usize..6, // at least one tick-path partition: a repair with none is refused at verify
                n_files in 0usize..4,
                crash_points in proptest::collection::vec(1u32..60, 0..4),
                rerun in any::<bool>(),
            ) {
                let (root, remote, files) = setup(n_parts, n_files);
                if rerun {
                    // The 2026-11-20 shape: a verify fails after a rebuild that
                    // wrote the tick partitions again.
                    let mut r = remote.borrow_mut();
                    r.rebuild_recreates = true;
                    r.fail_verifies = 1;
                }
                let payloads: Vec<Vec<u8>> = files
                    .iter()
                    .map(|f| std::fs::read(root.join(f)).expect("payload"))
                    .collect();
                drive(&root, &remote, &crash_points);
                let r = remote.borrow();
                prop_assert!(r.violations.is_empty(), "{:?}", r.violations);
                prop_assert!(r.rebuilds >= 1);
                if !rerun {
                    prop_assert!(r.partitions.is_empty());
                }
                // Never a rebuilt row read as the live day's, at any crash.
                for keys in &r.originals_seen {
                    prop_assert!(
                        keys.iter().all(|k| !k.contains(".sha-") && !k.contains("tick_quotes")),
                        "{keys:?}"
                    );
                    prop_assert_eq!(keys.len(), n_parts - 1);
                }
                for (_, pinned) in &r.coverage_seen {
                    prop_assert!(pinned
                        .values()
                        .flatten()
                        .all(|k| !k.contains(".sha-") && !k.contains("tick_quotes")));
                }
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
