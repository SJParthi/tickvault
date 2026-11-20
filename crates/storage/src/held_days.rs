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
//! exists.
//!
//! A released day's export catch-up window is counted from its `.done` date,
//! not from its own date, so a repair finished more than six days later still
//! exports ([`HeldDays::released_days`]).
//!
//! Both markers are one small text file with a CRC-32 over everything above
//! the `crc32=` line ([`encode_day_marker`]). Every doubt fails toward
//! HOLDING: a damaged or unreadable `.hold` file holds its day, and a damaged
//! or unreadable `.done` file releases nothing. A missing folder holds
//! nothing (for `.hold`) and releases nothing (for `.done`).
//!
//! Cost: one `stat`/read of at most two files of at most
//! [`DAY_MARKER_MAX_BYTES`] per day asked about, on cold paths only (once
//! per day per archive pass, once per day per export poll, once per past day
//! per cross-check pass); [`HeldDays::released_days`] lists one small
//! folder, O(files) capped at [`RELEASED_DAYS_MAX`]. Never on the frame
//! drain, never per tick.
//!
//! Honest limits: a `.hold` or `.done` file edited by hand with a matching
//! CRC is believed; the folders are relative to the process working
//! directory, as every other `data/state/*` file is.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::NaiveDate;

use tickvault_common::config::PartitionRetentionConfig;

/// Where `.hold` files live.
pub const HELD_DAYS_DIR: &str = "data/state/held-days";
/// Where the repair writes `.done` files.
pub const DAY_REPAIR_DIR: &str = "data/state/day-repair";
/// The largest marker file read; a longer file is damaged.
pub const DAY_MARKER_MAX_BYTES: u64 = 4096;
/// The most `.done` files [`HeldDays::released_days`] reads.
pub const RELEASED_DAYS_MAX: usize = 400;
/// First line of every marker.
const MARKER_MAGIC: &str = "tickvault-day-marker v1";
/// Longest reason kept in a marker, characters.
const REASON_MAX_CHARS: usize = 200;
/// The date format of file names and fields.
const DAY_FORMAT: &str = "%Y-%m-%d";

/// The two marker kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// `data/state/held-days/<day>.hold`: holds the day.
    Hold,
    /// `data/state/day-repair/<day>.done`: a verified repair released it.
    Done,
}

impl MarkerKind {
    /// The file suffix and the `kind=` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Done => "done",
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

/// The configured held days plus the two state folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldDays {
    configured: Vec<NaiveDate>,
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
/// `YYYY-MM-DD` date is skipped (config validation refuses it at boot).
/// Pure, O(entries).
#[must_use]
pub fn parse_held_days(entries: &[String]) -> Vec<NaiveDate> {
    entries
        .iter()
        .filter_map(|e| NaiveDate::parse_from_str(e.trim(), DAY_FORMAT).ok())
        .collect()
}

impl HeldDays {
    /// The process folders and `cfg.held_days`. O(entries).
    #[must_use]
    pub fn from_config(cfg: &PartitionRetentionConfig) -> Self {
        Self::with_dirs(
            parse_held_days(&cfg.held_days),
            PathBuf::from(HELD_DAYS_DIR),
            PathBuf::from(DAY_REPAIR_DIR),
        )
    }

    /// Explicit days and folders (tests, and the repair tool). O(1).
    #[must_use]
    pub fn with_dirs(configured: Vec<NaiveDate>, hold_dir: PathBuf, done_dir: PathBuf) -> Self {
        Self {
            configured,
            hold_dir,
            done_dir,
        }
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

    /// Whether, and why, `day` is held. At most two small file reads; the
    /// `.done` file is read only for a day the config or a `.hold` file holds.
    #[must_use]
    pub fn verdict(&self, day: NaiveDate) -> HeldVerdict {
        // O(1) EXEMPT: a handful of configured days.
        let configured = self.configured.contains(&day);
        let hold = read_day_marker(&self.hold_dir, MarkerKind::Hold, day);
        let source = match (configured, hold) {
            (true, _) => HeldSource::Config,
            (false, MarkerRead::Missing) => return HeldVerdict::NotHeld,
            (false, MarkerRead::Valid { .. }) => HeldSource::HoldFile,
            (false, MarkerRead::Damaged) => HeldSource::DamagedHoldFile,
        };
        match read_day_marker(&self.done_dir, MarkerKind::Done, day) {
            MarkerRead::Valid { written_on } => HeldVerdict::Released {
                done_on: written_on,
            },
            MarkerRead::Missing => HeldVerdict::Held {
                source,
                done_damaged: false,
            },
            MarkerRead::Damaged => HeldVerdict::Held {
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

    /// Whether the archiver keeps `partition` of `table` back: a held-kind
    /// table ([`table_is_held_kind`]) and a held day. An unparseable name is
    /// not held (the archiver never blocks on a name it cannot read).
    #[must_use]
    pub fn holds_partition(&self, table: &str, partition: &str) -> bool {
        table_is_held_kind(table) && partition_day(partition).is_some_and(|d| self.is_held(d))
    }

    /// Every day a valid `.done` file releases, with its `.done` date, oldest
    /// day first. One folder listing, at most [`RELEASED_DAYS_MAX`] files
    /// read; a missing or unreadable folder is an empty list.
    #[must_use]
    pub fn released_days(&self) -> Vec<(NaiveDate, NaiveDate)> {
        let Ok(entries) = std::fs::read_dir(&self.done_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten().take(RELEASED_DAYS_MAX) {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".done")) else {
                continue;
            };
            let Ok(day) = NaiveDate::parse_from_str(stem, DAY_FORMAT) else {
                continue;
            };
            if let MarkerRead::Valid { written_on } =
                read_day_marker(&self.done_dir, MarkerKind::Done, day)
            {
                out.push((day, written_on));
            }
        }
        out.sort_unstable();
        out
    }
}

/// The configured held days of this process, installed once at boot.
static PROCESS_HELD_DAYS: OnceLock<Vec<NaiveDate>> = OnceLock::new();

/// Installs `cfg.held_days` for [`process_held_days`]. The first call wins
/// (one config per process); called from the daily archive loop's spawn,
/// which `main` runs before the cross-check is spawned. O(entries).
pub fn install_process_held_days(cfg: &PartitionRetentionConfig) {
    if PROCESS_HELD_DAYS
        .set(parse_held_days(&cfg.held_days))
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
    HeldDays::with_dirs(
        PROCESS_HELD_DAYS.get().cloned().unwrap_or_default(),
        PathBuf::from(HELD_DAYS_DIR),
        PathBuf::from(DAY_REPAIR_DIR),
    )
}

#[cfg(test)]
#[path = "held_days_tests.rs"]
mod tests;
