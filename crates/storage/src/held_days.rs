//! Held days (daily-universe Quote 63, owner cards 2026-11-20).
//!
//! On 2026-11-20 Dhan sent every Last Trade Time 19,800 s early (UTC epoch
//! under the documented IST rule), so that day's `ticks`, `tick_quotes`,
//! `feed_aux_packets` and candle rows are wrong. A HELD day is kept out of
//! three paths until a verified repair releases it:
//!
//! 1. the partition archiver (day, hour and disk-pressure passes): it neither
//!    copies nor drops the day's `ticks`, `tick_quotes`, `feed_aux_packets`
//!    or `candles_*` partitions (`market_depth*` and the cross-verify tables
//!    are correct and archive as usual), counted on
//!    `tv_partition_archive_skipped_total{reason="held_day"}`;
//! 2. the daily Brutex export, forced and last-chance runs included (no
//!    attempt counted, no objects, no DONE);
//! 3. the daily cross-check's past-day catch-up.
//!
//! A day is held when it is listed in `[partition_retention] held_days`, or
//! when a state file `data/state/held-days/<YYYY-MM-DD>.hold` exists (the
//! feed-time page writes one, owner card D5). It is released only by a valid
//! `data/state/day-repair/<YYYY-MM-DD>.done` (written by the repair after its
//! verify passed), or by removing the config entry when no `.hold` file
//! exists. A `.done` dated after today (IST) releases nothing until that day
//! comes (a clock step, or a wrong date given to the tool).
//!
//! Release order (hostile review 2026-11-20): a released day's held-kind
//! partitions stay out of the ARCHIVER (not the export) until the export has
//! sealed the day and written `data/state/day-repair/<day>.exported`, or
//! [`RELEASE_EXPORT_WAIT_DAYS`] after the `.done` date, or at once when the
//! export is switched off. Without this the archiver, which sorts only
//! against an object already at the canonical key, would copy the repaired
//! hours UNORDERED before the export reached them (Quote 60 wants them in
//! DEDUP-key order).
//!
//! A released day's export catch-up window is counted from its `.done` date,
//! not from its own date, so a repair finished more than six days later still
//! exports ([`HeldDays::released_days`]).
//!
//! All markers are one small text file with a CRC-32 over everything above
//! the `crc32=` line ([`encode_day_marker`]). Every doubt about a DAY fails
//! toward HOLDING: a damaged or unreadable `.hold` file holds its day, and a
//! damaged or unreadable `.done` file releases nothing. A missing folder
//! holds nothing (for `.hold`) and releases nothing (for `.done`).
//!
//! Folder failures (hostile review 2026-11-20) are told apart from a damaged
//! day file and logged once per process with a coded `error!`
//! (`STORAGE-GAP-04`, log only, no page): a path that is not a folder holds
//! nothing (no marker can exist under it); a folder that cannot be read keeps
//! failing toward holding, which holds EVERY day, so the line names that.
//! A `held_days` entry that is not a date no longer stops the app at boot (it
//! would stop the live capture); it holds EVERY day instead, with one coded
//! `error!` per process, until the config is fixed.
//!
//! Cost: one `stat`/read of at most three files of at most
//! [`DAY_MARKER_MAX_BYTES`] per day asked about, on cold paths only (once
//! per day per archive pass and once per held-kind partition just before
//! its copy, once per day per export poll and per exported table, once per
//! past day per cross-check pass); a damaged read adds one folder `stat` and
//! one listing. [`HeldDays::released_days`] lists one small folder,
//! O(files) with at most [`RELEASED_DAYS_MAX`] markers read. Never on the
//! frame drain, never per tick.
//!
//! Honest limits:
//! - a marker edited by hand with a matching CRC is believed;
//! - the folders are relative to the process working directory
//!   (`/opt/tickvault` under systemd), as every other `data/state/*` file
//!   is: a repair tool run from another directory (an SSM shell) must pass
//!   the app's folders, or its `.done` is never seen and the day stays held;
//! - no production code writes a `.done` yet (the repair tool, PR-4); until
//!   then a hand hold is an EMPTY `data/state/held-days/<date>.hold` (it
//!   reads as damaged, which holds), released only by removing the file;
//! - a `.done` releases the export at once, whatever the binary: the repair
//!   tool must not write it before the manifest change (PR-5) is deployed;
//! - `market_depth*` of a held day is not held, so it is archived (unordered,
//!   no export object exists) long before the day's export runs, and the
//!   day's manifest lists depth as copies made before the export;
//! - the cross-check catch-up looks back 3 days, so a day released later is
//!   never checked, and from its 4th day the archive copies the day's
//!   partitions with the coded `STORAGE-GAP-04` "archived WITHOUT a
//!   cross-verification" line (log only) unless a marker already exists;
//! - a disk-pressure pass whose only candidates are held partitions frees
//!   nothing and counts toward the `STORAGE-GAP-05` escalation, which pages
//!   at or above high water (the disk really cannot be freed);
//! - a binary without this module (the one booted before a deploy swap)
//!   holds nothing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use chrono::NaiveDate;
use tracing::error;

use tickvault_common::config::PartitionRetentionConfig;
use tickvault_common::error_code::ErrorCode;

/// Where `.hold` files live.
pub const HELD_DAYS_DIR: &str = "data/state/held-days";
/// Where the repair writes `.done` files (and the export `.exported`).
pub const DAY_REPAIR_DIR: &str = "data/state/day-repair";
/// The largest marker file read; a longer file is damaged.
pub const DAY_MARKER_MAX_BYTES: u64 = 4096;
/// The most `.done` files [`HeldDays::released_days`] reads.
pub const RELEASED_DAYS_MAX: usize = 400;
/// Days after its `.done` date a released day's held-kind partitions wait
/// for the Brutex export (its `.exported` marker) before the archiver may
/// copy them anyway: the export's window from the `.done` date (7 days) plus
/// one. Bounds the wait when the export never seals the day.
pub const RELEASE_EXPORT_WAIT_DAYS: i64 = 8;
/// First line of every marker.
const MARKER_MAGIC: &str = "tickvault-day-marker v1";
/// Longest reason kept in a marker, characters.
const REASON_MAX_CHARS: usize = 200;
/// The date format of file names and fields.
const DAY_FORMAT: &str = "%Y-%m-%d";

/// The marker kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// `data/state/held-days/<day>.hold`: holds the day.
    Hold,
    /// `data/state/day-repair/<day>.done`: a verified repair released it.
    Done,
    /// `data/state/day-repair/<day>.exported`: the Brutex export sealed the
    /// released day, so the archiver may copy its partitions.
    Exported,
}

impl MarkerKind {
    /// The file suffix and the `kind=` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Done => "done",
            Self::Exported => "exported",
        }
    }

    /// Bit index for the once-per-process folder reports.
    const fn index(self) -> u8 {
        match self {
            Self::Hold => 0,
            Self::Done => 1,
            Self::Exported => 2,
        }
    }
}
/// What reading one marker file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerRead {
    /// No file (or no folder).
    Missing,
    /// A whole, checked marker written on `written_on` (IST date).
    Valid {
        /// The IST date the marker was written.
        written_on: NaiveDate,
    },
    /// The file exists but is torn, too long, unreadable, fails its CRC,
    /// or names another day or kind.
    Damaged,
}

/// Why a day is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldSource {
    /// Listed in `[partition_retention] held_days`.
    Config,
    /// A valid `.hold` file.
    HoldFile,
    /// A `.hold` file that could not be read or checked (held anyway).
    DamagedHoldFile,
    /// A `held_days` entry is not a date: every day is held until the
    /// config is fixed (one coded error per process).
    ConfigUnreadable,
}

/// The answer for one day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldVerdict {
    /// Not held: the three paths treat it as any other day.
    NotHeld,
    /// Held, with its source; `done_damaged` when a `.done` file exists but
    /// could not be checked (so it released nothing).
    Held {
        /// Why it is held.
        source: HeldSource,
        /// A `.done` file exists but is damaged.
        done_damaged: bool,
    },
    /// Was held, released by a valid `.done` written on `done_on`.
    Released {
        /// The IST date the `.done` was written.
        done_on: NaiveDate,
    },
}

impl HeldVerdict {
    /// Whether the day is held now. O(1).
    #[must_use]
    pub const fn is_held(self) -> bool {
        matches!(self, Self::Held { .. })
    }
}

/// Whether `table` is one a held day keeps back: `ticks`, `tick_quotes`,
/// `feed_aux_packets` and every `candles_*` table. `market_depth*` and the
/// cross-verify tables are not (their 20 Nov rows are correct). Pure, O(1).
#[must_use]
pub fn table_is_held_kind(table: &str) -> bool {
    matches!(table, "ticks" | "tick_quotes" | "feed_aux_packets") || table.starts_with("candles_")
}

/// The IST day of a QuestDB partition name (`YYYY-MM-DD` or
/// `YYYY-MM-DDTHH`), or `None` when unparseable. Pure, O(1).
#[must_use]
pub fn partition_day(partition: &str) -> Option<NaiveDate> {
    let day = partition.get(..10)?;
    let rest = partition.get(10..)?;
    if !(rest.is_empty() || rest.starts_with('T')) {
        return None;
    }
    NaiveDate::parse_from_str(day, DAY_FORMAT).ok()
}

/// The marker text for `day`, written on `written_on`, with `reason` (one
/// line, at most 200 characters). The last line is the CRC-32 of every
/// byte above it. Pure, O(reason).
#[must_use]
pub fn encode_day_marker(
    kind: MarkerKind,
    day: NaiveDate,
    written_on: NaiveDate,
    reason: &str,
) -> String {
    let reason: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(REASON_MAX_CHARS)
        .collect();
    let body = format!(
        "{MARKER_MAGIC}\nkind={}\nday={}\nwritten_ist={}\nreason={reason}\n",
        kind.as_str(),
        day.format(DAY_FORMAT),
        written_on.format(DAY_FORMAT),
    );
    let crc = crate::wal_applied_watermark::crc32_ieee(body.as_bytes());
    format!("{body}crc32={crc:08x}\n")
}

/// Checks marker `text` for `kind` and `day`. Pure, O(text).
#[must_use]
pub fn decode_day_marker(kind: MarkerKind, day: NaiveDate, text: &str) -> MarkerRead {
    decode_inner(kind, day, text).map_or(MarkerRead::Damaged, |written_on| MarkerRead::Valid {
        written_on,
    })
}

/// [`decode_day_marker`] as an `Option` of the written date.
fn decode_inner(kind: MarkerKind, day: NaiveDate, text: &str) -> Option<NaiveDate> {
    let body_end = text.rfind("crc32=")?;
    let (body, crc_line) = text.split_at(body_end);
    if !body.ends_with('\n') {
        return None;
    }
    let crc_text = crc_line.strip_prefix("crc32=")?.strip_suffix('\n')?;
    if crc_text.len() != 8 {
        return None;
    }
    let crc = u32::from_str_radix(crc_text, 16).ok()?;
    if crc != crate::wal_applied_watermark::crc32_ieee(body.as_bytes()) {
        return None;
    }
    let mut lines = body.lines();
    if lines.next()? != MARKER_MAGIC {
        return None;
    }
    if lines.next()?.strip_prefix("kind=")? != kind.as_str() {
        return None;
    }
    let named = NaiveDate::parse_from_str(lines.next()?.strip_prefix("day=")?, DAY_FORMAT).ok()?;
    if named != day {
        return None;
    }
    let written_on =
        NaiveDate::parse_from_str(lines.next()?.strip_prefix("written_ist=")?, DAY_FORMAT).ok()?;
    lines.next()?.strip_prefix("reason=")?;
    if lines.next().is_some() || written_on < day {
        return None;
    }
    Some(written_on)
}

/// The marker file path for `day` in `dir`.
#[must_use]
pub fn day_marker_path(dir: &Path, kind: MarkerKind, day: NaiveDate) -> PathBuf {
    dir.join(format!("{}.{}", day.format(DAY_FORMAT), kind.as_str()))
}

/// Reads and checks the marker for `day` in `dir`. A missing file or folder
/// is [`MarkerRead::Missing`]; any other read failure, a file over
/// [`DAY_MARKER_MAX_BYTES`] or a failed check is [`MarkerRead::Damaged`].
/// One open and one read of at most 4 KiB.
#[must_use]
pub fn read_day_marker(dir: &Path, kind: MarkerKind, day: NaiveDate) -> MarkerRead {
    use std::io::Read;
    let path = day_marker_path(dir, kind, day);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return MarkerRead::Missing,
        Err(_) => return MarkerRead::Damaged,
    };
    let mut text = String::new();
    match file
        .take(DAY_MARKER_MAX_BYTES.saturating_add(1))
        .read_to_string(&mut text)
    {
        Ok(n) if (n as u64) <= DAY_MARKER_MAX_BYTES => decode_day_marker(kind, day, &text),
        Ok(_) | Err(_) => MarkerRead::Damaged,
    }
}

/// Writes the marker for `day` in `dir` (creating the folder): a temporary
/// file, `sync_all`, rename, then a sync of the folder, so a crash leaves the
/// old file or the new one, never a torn one. Cold, a few syscalls.
///
/// # Errors
/// Any failure to create, write, sync or rename (the old file, if any, is
/// left as it was).
pub fn write_day_marker(
    dir: &Path,
    kind: MarkerKind,
    day: NaiveDate,
    written_on: NaiveDate,
    reason: &str,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = day_marker_path(dir, kind, day);
    let tmp = path.with_extension(format!("{}.tmp", kind.as_str()));
    let text = encode_day_marker(kind, day, written_on, reason);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    std::fs::File::open(dir)?.sync_all()
}

/// Why a marker read as [`MarkerRead::Damaged`]: its own file, or its folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamagedCause {
    /// The day's own file (or a case that cannot be told apart): the read
    /// stays damaged.
    File,
    /// The folder path (or a folder above it) is not a folder: no marker
    /// can exist under it, so the read is [`MarkerRead::Missing`].
    FolderNotADirectory,
    /// The folder cannot be read (permissions, I/O): the read stays damaged
    /// (holds), which holds every day asked about.
    FolderUnreadable,
}

/// Tells a damaged marker read apart from a folder failure: one `stat` of
/// `dir` and, when it is a folder, one listing to see that it can be read.
/// Cold, only after a damaged read.
#[must_use]
pub fn damaged_marker_cause(dir: &Path) -> DamagedCause {
    match std::fs::metadata(dir) {
        Ok(meta) if !meta.is_dir() => DamagedCause::FolderNotADirectory,
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => {
            DamagedCause::FolderNotADirectory
        }
        Ok(_) => match std::fs::read_dir(dir) {
            Ok(_) => DamagedCause::File,
            Err(_) => DamagedCause::FolderUnreadable,
        },
        Err(_) => DamagedCause::FolderUnreadable,
    }
}

/// Folder failures already reported, one bit per (kind, cause).
static FOLDER_FAILURES_REPORTED: AtomicU8 = AtomicU8::new(0);

/// Logs a folder failure once per process per (kind, cause), with a coded
/// `error!` (log only). Returns whether this call logged it. O(1).
fn report_folder_failure(dir: &Path, kind: MarkerKind, cause: DamagedCause) -> bool {
    let cause_bit = match cause {
        DamagedCause::File => return false,
        DamagedCause::FolderNotADirectory => 0,
        DamagedCause::FolderUnreadable => 1,
    };
    let bit = 1_u8 << (kind.index() * 2 + cause_bit);
    if FOLDER_FAILURES_REPORTED.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return false;
    }
    let effect = match (kind, cause) {
        (MarkerKind::Hold, DamagedCause::FolderUnreadable) => {
            "EVERY day reads as HELD (its tick, quote, aux and candle partitions are not \
             archived and no Brutex export runs) until the folder can be read"
        }
        (MarkerKind::Hold, _) => {
            "no `.hold` file can exist there, so only the configured held days are held"
        }
        (MarkerKind::Done, _) => "no repaired day can be released until it is fixed",
        (MarkerKind::Exported, _) => {
            "a released day waits up to 8 days before the archiver copies it"
        }
    };
    error!(
        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
        source = "held_days_folder_unreadable",
        folder = %dir.display(),
        kind = kind.as_str(),
        cause = ?cause,
        "held days (Quote 63): this state folder cannot be used as a folder; {effect}. \
         Fix the folder's type or permissions for the app user"
    );
    true
}

/// [`read_day_marker`], with a folder failure told apart ([`DamagedCause`])
/// and reported once per process. Cold.
#[must_use]
pub fn read_day_marker_checked(dir: &Path, kind: MarkerKind, day: NaiveDate) -> MarkerRead {
    match read_day_marker(dir, kind, day) {
        MarkerRead::Damaged => {
            let cause = damaged_marker_cause(dir);
            let _ = report_folder_failure(dir, kind, cause);
            if cause == DamagedCause::FolderNotADirectory {
                MarkerRead::Missing
            } else {
                MarkerRead::Damaged
            }
        }
        other => other,
    }
}

/// Today's IST date from the wall clock. O(1).
#[must_use]
pub fn today_ist() -> NaiveDate {
    chrono::Utc::now()
        .with_timezone(&tickvault_common::trading_calendar::ist_offset())
        .date_naive()
}

/// The configured held days plus the two state folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldDays {
    configured: Vec<NaiveDate>,
    /// A `held_days` entry is not a date: every day is held.
    config_unreadable: bool,
    /// The Brutex export is on: a released day's held-kind partitions wait
    /// for its `.exported` marker before the archiver copies them.
    export_first: bool,
    /// Today's IST date for tests; `None` reads the wall clock.
    today: Option<NaiveDate>,
    hold_dir: PathBuf,
    done_dir: PathBuf,
}

impl Default for HeldDays {
    fn default() -> Self {
        Self::with_dirs(
            Vec::new(),
            PathBuf::from(HELD_DAYS_DIR),
            PathBuf::from(DAY_REPAIR_DIR),
        )
    }
}

/// Parses the `held_days` config entries; an entry that is not a
/// `YYYY-MM-DD` date is skipped here ([`held_days_config_unreadable`] makes
/// the whole list hold every day). Pure, O(entries).
#[must_use]
pub fn parse_held_days(entries: &[String]) -> Vec<NaiveDate> {
    entries
        .iter()
        .filter_map(|e| NaiveDate::parse_from_str(e.trim(), DAY_FORMAT).ok())
        .collect()
}

/// Whether any `held_days` entry is not a `YYYY-MM-DD` date. Pure,
/// O(entries).
#[must_use]
pub fn held_days_config_unreadable(entries: &[String]) -> bool {
    entries
        .iter()
        .any(|e| NaiveDate::parse_from_str(e.trim(), DAY_FORMAT).is_err())
}

/// Reported once per process: a `held_days` entry that is not a date.
static CONFIG_UNREADABLE_REPORTED: AtomicBool = AtomicBool::new(false);

/// Logs, once per process, that a `held_days` entry is not a date and every
/// day is held. Returns whether this call logged it. O(entries).
fn report_config_unreadable(entries: &[String]) -> bool {
    if CONFIG_UNREADABLE_REPORTED.swap(true, Ordering::Relaxed) {
        return false;
    }
    error!(
        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
        source = "held_days_config_unreadable",
        entries = ?entries,
        "held days (Quote 63): a [partition_retention] held_days entry is not a YYYY-MM-DD \
         date, so EVERY day is held (no tick, quote, aux or candle partition is archived and \
         no Brutex export runs) until the config is fixed. The live capture is not affected"
    );
    true
}

impl HeldDays {
    /// The process folders and `cfg.held_days`; the export-first wait is on
    /// when `cfg.brutex_export_enabled`. An unreadable entry holds every day
    /// (reported once per process). O(entries).
    #[must_use]
    pub fn from_config(cfg: &PartitionRetentionConfig) -> Self {
        let mut held = Self::with_dirs(
            parse_held_days(&cfg.held_days),
            PathBuf::from(HELD_DAYS_DIR),
            PathBuf::from(DAY_REPAIR_DIR),
        );
        held.export_first = cfg.brutex_export_enabled;
        held.config_unreadable = held_days_config_unreadable(&cfg.held_days);
        if held.config_unreadable {
            let _ = report_config_unreadable(&cfg.held_days);
        }
        held
    }

    /// Explicit days and folders (tests, and the repair tool); the
    /// export-first wait is off. O(1).
    #[must_use]
    pub fn with_dirs(configured: Vec<NaiveDate>, hold_dir: PathBuf, done_dir: PathBuf) -> Self {
        Self {
            configured,
            config_unreadable: false,
            export_first: false,
            today: None,
            hold_dir,
            done_dir,
        }
    }

    /// Sets the export-first wait ([`RELEASE_EXPORT_WAIT_DAYS`]). O(1).
    #[must_use]
    pub fn with_export_first(mut self, export_first: bool) -> Self {
        self.export_first = export_first;
        self
    }

    /// Pins today's IST date (tests, and the repair tool's dry run). O(1).
    #[must_use]
    pub fn with_today(mut self, today: NaiveDate) -> Self {
        self.today = Some(today);
        self
    }

    /// The `.hold` folder.
    #[must_use]
    pub fn hold_dir(&self) -> &Path {
        &self.hold_dir
    }

    /// The `.done` folder.
    #[must_use]
    pub fn done_dir(&self) -> &Path {
        &self.done_dir
    }

    /// Today's IST date: the pinned one, else the wall clock. O(1).
    fn today(&self) -> NaiveDate {
        self.today.unwrap_or_else(today_ist)
    }

    /// Whether, and why, `day` is held. At most two small file reads; the
    /// `.done` file is read only for a day the config or a `.hold` file
    /// holds. A `.done` dated after today releases nothing (held, with
    /// `done_damaged`).
    #[must_use]
    pub fn verdict(&self, day: NaiveDate) -> HeldVerdict {
        // O(1) EXEMPT: a handful of configured days.
        let listed = self.configured.contains(&day);
        let hold = read_day_marker_checked(&self.hold_dir, MarkerKind::Hold, day);
        let source = match (listed, self.config_unreadable, hold) {
            (true, _, _) => HeldSource::Config,
            (false, true, _) => HeldSource::ConfigUnreadable,
            (false, false, MarkerRead::Missing) => return HeldVerdict::NotHeld,
            (false, false, MarkerRead::Valid { .. }) => HeldSource::HoldFile,
            (false, false, MarkerRead::Damaged) => HeldSource::DamagedHoldFile,
        };
        match read_day_marker_checked(&self.done_dir, MarkerKind::Done, day) {
            MarkerRead::Valid { written_on } if written_on <= self.today() => {
                HeldVerdict::Released {
                    done_on: written_on,
                }
            }
            MarkerRead::Missing => HeldVerdict::Held {
                source,
                done_damaged: false,
            },
            MarkerRead::Valid { .. } | MarkerRead::Damaged => HeldVerdict::Held {
                source,
                done_damaged: true,
            },
        }
    }

    /// Whether `day` is held now. See [`Self::verdict`].
    #[must_use]
    pub fn is_held(&self, day: NaiveDate) -> bool {
        self.verdict(day).is_held()
    }

    /// Whether the ARCHIVER keeps `day` back under `verdict`: a held day, or
    /// a repaired day while the export-first wait runs (no valid
    /// `.exported` marker, at most [`RELEASE_EXPORT_WAIT_DAYS`] after its
    /// `.done` date). A day no longer held at all (its config entry removed,
    /// PR-6) but with a valid `.done` waits the same way, so removing the
    /// entry early never lets the archiver go first. One or two more small
    /// file reads, only with the export on.
    #[must_use]
    pub fn archive_holds(&self, day: NaiveDate, verdict: HeldVerdict) -> bool {
        let done_on = match verdict {
            HeldVerdict::Held { .. } => return true,
            HeldVerdict::Released { done_on } => done_on,
            HeldVerdict::NotHeld => {
                if !self.export_first {
                    return false;
                }
                match read_day_marker(&self.done_dir, MarkerKind::Done, day) {
                    MarkerRead::Valid { written_on } if written_on <= self.today() => written_on,
                    _ => return false,
                }
            }
        };
        self.export_first
            && done_on
                .checked_add_signed(chrono::TimeDelta::days(RELEASE_EXPORT_WAIT_DAYS))
                .is_some_and(|until| self.today() <= until)
            && !matches!(
                read_day_marker_checked(&self.done_dir, MarkerKind::Exported, day),
                MarkerRead::Valid { .. }
            )
    }

    /// The day's verdict and whether the archiver keeps it back
    /// ([`Self::archive_holds`]). The archive pass and the drain-hold
    /// backlog both decide through this. At most three small file reads.
    #[must_use]
    pub fn archive_verdict(&self, day: NaiveDate) -> (HeldVerdict, bool) {
        let verdict = self.verdict(day);
        (verdict, self.archive_holds(day, verdict))
    }

    /// Whether the archiver keeps `partition` of `table` back: a held-kind
    /// table ([`table_is_held_kind`]) and a day [`Self::archive_verdict`]
    /// keeps back. An unparseable name is not held (the archiver never
    /// blocks on a name it cannot read).
    #[must_use]
    pub fn holds_partition(&self, table: &str, partition: &str) -> bool {
        table_is_held_kind(table)
            && partition_day(partition).is_some_and(|d| self.archive_verdict(d).1)
    }

    /// Records that the Brutex export sealed `day`, when a repair released
    /// it (a valid `.done` dated today or earlier), so the archiver may copy
    /// its partitions: writes a valid `.exported` marker dated today unless
    /// one is there. Returns whether a marker was written. A day with no
    /// valid `.done` writes nothing. Cold.
    ///
    /// # Errors
    /// The marker could not be written (the archiver then waits up to
    /// [`RELEASE_EXPORT_WAIT_DAYS`]).
    pub fn note_exported(&self, day: NaiveDate) -> std::io::Result<bool> {
        let today = self.today();
        if !matches!(
            read_day_marker(&self.done_dir, MarkerKind::Done, day),
            MarkerRead::Valid { written_on } if written_on <= today
        ) {
            return Ok(false);
        }
        if matches!(
            read_day_marker(&self.done_dir, MarkerKind::Exported, day),
            MarkerRead::Valid { .. }
        ) {
            return Ok(false);
        }
        write_day_marker(
            &self.done_dir,
            MarkerKind::Exported,
            day,
            today.max(day),
            "brutex export sealed",
        )?;
        Ok(true)
    }

    /// Every day a valid `.done` file releases, with its `.done` date, oldest
    /// day first. One folder listing; at most [`RELEASED_DAYS_MAX`] `.done`
    /// files read (other files never count toward it); a `.done` dated after
    /// today is left out; a missing or unreadable folder is an empty list.
    #[must_use]
    pub fn released_days(&self) -> Vec<(NaiveDate, NaiveDate)> {
        let Ok(entries) = std::fs::read_dir(&self.done_dir) else {
            return Vec::new();
        };
        let today = self.today();
        let mut out = Vec::new();
        let days = entries.flatten().filter_map(|entry| {
            let name = entry.file_name();
            let stem = name.to_str()?.strip_suffix(".done")?.to_string();
            NaiveDate::parse_from_str(&stem, DAY_FORMAT).ok()
        });
        for day in days.take(RELEASED_DAYS_MAX) {
            if let MarkerRead::Valid { written_on } =
                read_day_marker(&self.done_dir, MarkerKind::Done, day)
                && written_on <= today
            {
                out.push((day, written_on));
            }
        }
        out.sort_unstable();
        out
    }
}

/// The configured held days of this process, installed once at boot, with
/// whether an entry was unreadable.
static PROCESS_HELD_DAYS: OnceLock<(Vec<NaiveDate>, bool)> = OnceLock::new();

/// Installs `cfg.held_days` for [`process_held_days`]. The first call wins
/// (one config per process); called from the daily archive loop's spawn,
/// which `main` runs before the cross-check is spawned. O(entries).
pub fn install_process_held_days(cfg: &PartitionRetentionConfig) {
    let unreadable = held_days_config_unreadable(&cfg.held_days);
    if unreadable {
        let _ = report_config_unreadable(&cfg.held_days);
    }
    if PROCESS_HELD_DAYS
        .set((parse_held_days(&cfg.held_days), unreadable))
        .is_err()
    {
        // Already installed: one config per process, the first call wins.
    }
}

/// The process held days ([`install_process_held_days`]) with the process
/// folders, for a path that has no config of its own (the cross-check). Not
/// installed: no configured day (the `.hold` and `.done` files still count).
#[must_use]
pub fn process_held_days() -> HeldDays {
    let (configured, unreadable) = PROCESS_HELD_DAYS.get().cloned().unwrap_or_default();
    let mut held = HeldDays::with_dirs(
        configured,
        PathBuf::from(HELD_DAYS_DIR),
        PathBuf::from(DAY_REPAIR_DIR),
    );
    held.config_unreadable = unreadable;
    held
}

#[cfg(test)]
#[path = "held_days_tests.rs"]
mod tests;
