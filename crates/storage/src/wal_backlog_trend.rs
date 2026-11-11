//! Is the WAL replay backlog shrinking between two starts? (noise lock §2.20,
//! 2026-11-11, owner card "Page only if stuck (Recommended)").
//!
//! The per-boot frame cap and the byte budget end almost every start's replay
//! with segments left while a large backlog drains one slice per start. Those
//! lines lose nothing (a deferred segment stays a `*.wal` file), so the owner
//! wants a page only when the backlog is STUCK: when the count of waiting
//! segments did not fall since the previous start.
//!
//! Once per process, after the WAL folder is claimed and before the boot
//! replay reads anything, [`judge_at_boot`] counts the waiting segments the
//! replay would still read (`wal_unapplied_ranges::waiting_unreplayed_segments`),
//! compares the count with the one the previous recorded start saved in
//! [`WAL_BACKLOG_TREND_STATE_PATH`], saves its own count, and keeps the
//! verdict for the process. `wal_replay_frame_cap` reads it through
//! [`boot_backlog_trend`] to decide whether a replay-limit line pages.
//!
//! The planned evening-drain restart does NOT save its count (2026-11-11
//! review fix, noise lock §2.20 note). It is always quiet anyway (§2.17), and
//! its count is taken after the trading day's growth but before its own
//! replay, so saving it let the next morning compare against a count that
//! held the evening's progress and none of the day's growth: a backlog that
//! grew a little every day never paged. So every other start compares with
//! the previous start that was NOT a planned evening restart.
//!
//! The record is one line, `v2 <IST date> <count|unknown> <crc32 hex>`, the
//! checksum over everything before it, written with a temp file, `sync_all`,
//! rename and a folder sync. A line whose checksum does not match is
//! unreadable, so a corrupted digit pages instead of reading as a higher
//! previous count. A start that cannot save its record pages itself
//! ([`BacklogTrend::StateNotSaved`]) and removes the old record if it can; a
//! folder that stays unwritable therefore pages on every start, never
//! compares against a frozen count quietly.
//!
//! Only a STRICT fall with both counts known, from a record not dated after
//! today, saved by this start, reads as shrinking. Everything else (first
//! start, unreadable record, unknown count now or then, equal or higher
//! count, a record from the future, a save that failed) keeps the page.
//!
//! A count taken without a trusted, bounded applied watermark (absent,
//! rejected, or an overflow of unknown height) while segments wait is saved
//! as `unknown` (2026-11-11 review fix): it counts applied segments too, so
//! the next normal start must not compare against it.
//!
//! Cost, once per process, cold, never on the frame path: one span scan of
//! the waiting segments (one header read per segment, plus a 64 KiB tail
//! read, at most 4 MiB, of each segment followed by a sequence gap larger
//! than one bucket; the boot fold runs its own scan too), and one small file
//! read and write. It runs through `off_worker`, so it does not hold a
//! runtime worker.

use std::io::Write as _;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_common::error_code::ErrorCode;
use tracing::{info, warn};

/// Where the previous start's backlog count is kept.
pub const WAL_BACKLOG_TREND_STATE_PATH: &str = "data/state/wal-backlog-trend.state";

/// The record's version word (`v2` adds the checksum; a `v1` line from the
/// unshipped first draft reads as unreadable and pages once).
const RECORD_VERSION: &str = "v2";

/// Hex digits of the record's CRC-32 word.
const CRC_HEX_LEN: usize = 8;

/// A record longer than this is refused unread (a real one is ~25 bytes).
const MAX_RECORD_BYTES: u64 = 128;

/// The word written for an unknown count.
const UNKNOWN_COUNT: &str = "unknown";

/// IST offset from UTC, seconds.
const IST_OFFSET_SECS: i64 = 19_800;

/// Seconds in a day.
const SECS_PER_DAY: i64 = 86_400;

/// One saved start: its IST day (days since 1970-01-01 IST) and the waiting
/// segment count it saw (`None` when it could not count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BacklogRecord {
    pub(crate) ist_day: i64,
    pub(crate) segments: Option<u64>,
}

/// The IST day number of a UTC unix time. Pure, O(1).
#[must_use]
pub(crate) const fn ist_day_of_unix(unix_secs: i64) -> i64 {
    (unix_secs + IST_OFFSET_SECS).div_euclid(SECS_PER_DAY)
}

fn epoch_date() -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
}

/// The CRC-32 (IEEE) of a record's body, the line before its checksum word.
fn record_crc(body: &str) -> u32 {
    crate::ws_frame_spill::crc32_ieee_of(&[body.as_bytes()])
}

impl BacklogRecord {
    /// The record line, `v2 <yyyy-mm-dd> <count|unknown> <crc32 hex>\n`.
    /// `None` only for a day outside the calendar's range. Cold.
    pub(crate) fn to_line(self) -> Option<String> {
        let date = epoch_date()?.checked_add_signed(chrono::TimeDelta::try_days(self.ist_day)?)?;
        let count = self
            .segments
            .map_or_else(|| UNKNOWN_COUNT.to_string(), |n| n.to_string());
        let body = format!("{RECORD_VERSION} {} {count}", date.format("%Y-%m-%d"));
        let crc = record_crc(&body);
        Some(format!("{body} {crc:08x}\n"))
    }

    /// Strict parse of one record line, checksum included; anything else is
    /// `None`. Cold.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let line = text.strip_suffix('\n')?;
        let (body, crc_word) = line.rsplit_once(' ')?;
        if crc_word.len() != CRC_HEX_LEN
            || !crc_word
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || u32::from_str_radix(crc_word, 16).ok()? != record_crc(body)
        {
            return None;
        }
        let mut words = body.split(' ');
        if words.next()? != RECORD_VERSION {
            return None;
        }
        let date_word = words.next()?;
        let count_word = words.next()?;
        if words.next().is_some() || date_word.len() != 10 {
            return None;
        }
        let date = chrono::NaiveDate::parse_from_str(date_word, "%Y-%m-%d").ok()?;
        let ist_day = date.signed_duration_since(epoch_date()?).num_days();
        let segments = if count_word == UNKNOWN_COUNT {
            None
        } else {
            if count_word.is_empty() || !count_word.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(count_word.parse::<u64>().ok()?)
        };
        Some(Self { ist_day, segments })
    }
}

/// What a start found in the record file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousRecord {
    /// No file: the first start (or the last write failed and removed it).
    Absent,
    /// A file that could not be read or parsed.
    Unreadable,
    /// A well-formed record.
    Valid(BacklogRecord),
}

/// Reads the record at `path`. Cold, at most [`MAX_RECORD_BYTES`].
pub(crate) fn read_backlog_record(path: &Path) -> PreviousRecord {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return PreviousRecord::Absent,
        Err(_) => return PreviousRecord::Unreadable,
        Ok(meta) if !meta.file_type().is_file() || meta.len() > MAX_RECORD_BYTES => {
            return PreviousRecord::Unreadable;
        }
        Ok(_) => {}
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| BacklogRecord::parse(&text))
        .map_or(PreviousRecord::Unreadable, PreviousRecord::Valid)
}

/// A failed save: the error, and whether the rename had already put this
/// start's record at the path (only the final folder sync failed).
#[derive(Debug)]
pub(crate) struct StoreFailure {
    pub(crate) error: std::io::Error,
    pub(crate) renamed: bool,
}

impl StoreFailure {
    fn before_rename(error: std::io::Error) -> Self {
        Self {
            error,
            renamed: false,
        }
    }
}

/// The folder holding `path` (`.` for a bare name). Pure.
fn parent_folder(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Syncs a folder so a rename or delete in it survives a power cut. Cold.
fn sync_folder(folder: &Path) -> std::io::Result<()> {
    std::fs::File::open(folder)?.sync_all()
}

/// Writes `record` to `path`: temp file, `sync_all`, rename, folder sync.
/// Creates the folder. Cold.
///
/// # Errors
/// Any step failed; the caller removes whatever record is at the path.
/// Production calls [`store_backlog_record_with`] through `judge_at_boot_in`.
#[cfg(test)]
pub(crate) fn store_backlog_record(path: &Path, record: BacklogRecord) -> Result<(), StoreFailure> {
    store_backlog_record_with(path, record, sync_folder)
}

/// Writes `record` to `path`: temp file, `sync_all`, rename, then the
/// folder sync `sync_dir` (tests make it fail after the rename). Cold.
fn store_backlog_record_with(
    path: &Path,
    record: BacklogRecord,
    sync_dir: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<(), StoreFailure> {
    let line = record.to_line().ok_or_else(|| {
        StoreFailure::before_rename(std::io::Error::other("backlog record day out of range"))
    })?;
    let parent = parent_folder(path);
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    };
    write().map_err(StoreFailure::before_rename)?;
    sync_dir(parent).map_err(|error| StoreFailure {
        error,
        renamed: true,
    })
}

/// Which record a failed save removed, for the boot log. Pure, O(1).
#[must_use]
pub(crate) const fn removed_record_label(renamed: bool) -> &'static str {
    if renamed {
        "this_start"
    } else {
        "previous_start"
    }
}

/// The trend between the previous start and this one. Only `Shrinking`
/// quiets a replay-limit line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklogTrend {
    /// Both counts known and the current one is strictly lower.
    Shrinking { previous: u64, current: u64 },
    /// No previous record.
    FirstStart,
    /// The previous record could not be read.
    Unreadable,
    /// The previous record is dated after today (a clock step back).
    FutureDated,
    /// The previous start could not count.
    PreviousUnknown,
    /// This start could not count.
    CurrentUnknown,
    /// The same count as the previous start.
    Unchanged { previous: u64, current: u64 },
    /// A higher count than the previous start.
    Grew { previous: u64, current: u64 },
    /// This start could not save its own record, so the record on disk may
    /// be a frozen older count; pages whatever the comparison said.
    StateNotSaved,
}

impl BacklogTrend {
    /// `true` only for [`Self::Shrinking`]. O(1).
    #[must_use]
    pub const fn is_shrinking(self) -> bool {
        matches!(self, Self::Shrinking { .. })
    }

    /// A short label for the boot log. O(1).
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Shrinking { .. } => "shrinking",
            Self::FirstStart => "first_start",
            Self::Unreadable => "record_unreadable",
            Self::FutureDated => "record_future_dated",
            Self::PreviousUnknown => "previous_unknown",
            Self::CurrentUnknown => "current_unknown",
            Self::Unchanged { .. } => "unchanged",
            Self::Grew { .. } => "grew",
            Self::StateNotSaved => "state_not_saved",
        }
    }

    /// `(previous, current)` when both are known. O(1).
    #[must_use]
    pub const fn counts(self) -> Option<(u64, u64)> {
        match self {
            Self::Shrinking { previous, current }
            | Self::Unchanged { previous, current }
            | Self::Grew { previous, current } => Some((previous, current)),
            _ => None,
        }
    }
}

/// The pure §2.20 decision. Pure, O(1).
#[must_use]
pub(crate) const fn backlog_trend_verdict(
    previous: PreviousRecord,
    today_ist_day: i64,
    current: Option<u64>,
) -> BacklogTrend {
    let Some(current) = current else {
        return BacklogTrend::CurrentUnknown;
    };
    let record = match previous {
        PreviousRecord::Absent => return BacklogTrend::FirstStart,
        PreviousRecord::Unreadable => return BacklogTrend::Unreadable,
        PreviousRecord::Valid(record) => record,
    };
    if record.ist_day > today_ist_day {
        return BacklogTrend::FutureDated;
    }
    let Some(previous) = record.segments else {
        return BacklogTrend::PreviousUnknown;
    };
    if current < previous {
        BacklogTrend::Shrinking { previous, current }
    } else if current == previous {
        BacklogTrend::Unchanged { previous, current }
    } else {
        BacklogTrend::Grew { previous, current }
    }
}

/// What one start decided, kept for the boot log.
#[derive(Debug, Clone)]
pub(crate) struct BootJudgement {
    /// The verdict the replay-limit lines read.
    pub(crate) trend: BacklogTrend,
    /// What the comparison alone said (differs from `trend` only when the
    /// save failed); its counts go in the boot log.
    pub(crate) compared: BacklogTrend,
    /// The planned evening-drain restart: its count was not saved.
    pub(crate) record_skipped_planned: bool,
    /// The record write failed.
    pub(crate) state_write_error: Option<String>,
    /// After a failed write: no record file is left at the path.
    pub(crate) record_removed: bool,
    /// After a failed write: which record the removal took,
    /// [`removed_record_label`] (this start's when the rename had landed and
    /// only the folder sync failed, else the previous start's).
    pub(crate) removed_record: Option<&'static str>,
    /// After a removal: its folder sync succeeded. When `false` a power cut
    /// can bring a record back (the next start then compares against it).
    pub(crate) removal_synced: bool,
}

/// The verdict once the save is known: a start that could not save its
/// record pages ([`BacklogTrend::StateNotSaved`]), whatever it compared,
/// because the record it read may itself be a count frozen by an earlier
/// failed save. Pure, O(1).
#[must_use]
pub(crate) const fn trend_after_save(compared: BacklogTrend, saved: bool) -> BacklogTrend {
    if saved {
        compared
    } else {
        BacklogTrend::StateNotSaved
    }
}

/// One start's judgement against explicit paths and day: count, compare,
/// and (unless `planned_evening_restart`) save this start's count, removing
/// whatever record is at the path when the save fails. Cold.
pub(crate) fn judge_at_boot_in(
    wal_dir: &Path,
    state_path: &Path,
    today_ist_day: i64,
    planned_evening_restart: bool,
) -> BootJudgement {
    judge_at_boot_in_with(
        wal_dir,
        state_path,
        today_ist_day,
        planned_evening_restart,
        sync_folder,
    )
}

/// [`judge_at_boot_in`] with the folder sync passed in (tests make it fail).
/// Cold.
fn judge_at_boot_in_with(
    wal_dir: &Path,
    state_path: &Path,
    today_ist_day: i64,
    planned_evening_restart: bool,
    sync_dir: impl Fn(&Path) -> std::io::Result<()> + Copy,
) -> BootJudgement {
    let current = crate::wal_unapplied_ranges::waiting_unreplayed_segments(wal_dir);
    let previous = read_backlog_record(state_path);
    let compared = backlog_trend_verdict(previous, today_ist_day, current);
    let unsaved = |skipped| BootJudgement {
        trend: compared,
        compared,
        record_skipped_planned: skipped,
        state_write_error: None,
        record_removed: false,
        removed_record: None,
        removal_synced: false,
    };
    if planned_evening_restart {
        return unsaved(true);
    }
    let record = BacklogRecord {
        ist_day: today_ist_day,
        segments: current,
    };
    match store_backlog_record_with(state_path, record, sync_dir) {
        Ok(()) => BootJudgement {
            trend: trend_after_save(compared, true),
            ..unsaved(false)
        },
        Err(failure) => {
            let removed = match std::fs::remove_file(state_path) {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            };
            let synced = removed && sync_dir(parent_folder(state_path)).is_ok();
            BootJudgement {
                trend: trend_after_save(compared, false),
                state_write_error: Some(failure.error.to_string()),
                record_removed: removed,
                removed_record: removed.then_some(removed_record_label(failure.renamed)),
                removal_synced: synced,
                ..unsaved(false)
            }
        }
    }
}

/// This process's judgement, decided once by [`judge_at_boot`].
static BOOT_JUDGEMENT: OnceLock<BootJudgement> = OnceLock::new();

/// Set once [`log_boot_backlog_trend`] wrote its lines.
static BOOT_JUDGEMENT_LOGGED: AtomicBool = AtomicBool::new(false);

/// Judges this process's start once (noise lock §2.20): counts the waiting
/// segments, compares with the previous recorded start's saved count, saves
/// this one (not on the planned evening-drain restart, whose claim `main`
/// records before this runs), and keeps the verdict for the replay-limit
/// lines. Called from `main` after the WAL folder is claimed and before the
/// boot replay. Idempotent: the first call decides. The scan and the file
/// write run through `off_worker`, so they do not hold a runtime worker.
/// Cold.
// TEST-EXEMPT: a OnceLock around judge_at_boot_in (unit-tested: test_judge_at_boot_in_*); its placement after the planned-restart claim and the WAL lock and before the boot replay is pinned by the_lane_and_main_are_wired_to_the_evening_drain.
pub fn judge_at_boot(wal_dir: &Path) -> BacklogTrend {
    BOOT_JUDGEMENT
        .get_or_init(|| {
            let today = ist_day_of_unix(chrono::Utc::now().timestamp());
            crate::off_worker::off_worker(|| {
                judge_at_boot_in(
                    wal_dir,
                    Path::new(WAL_BACKLOG_TREND_STATE_PATH),
                    today,
                    crate::wal_replay_frame_cap::planned_evening_drain_boot(),
                )
            })
        })
        .trend
}

/// This process's backlog trend, `None` before [`judge_at_boot`] ran (a test
/// binary, or a lane started without it): such a start pages. O(1).
#[must_use]
pub fn boot_backlog_trend() -> Option<BacklogTrend> {
    BOOT_JUDGEMENT.get().map(|j| j.trend)
}

/// Writes the boot lines for `judgement`. Cold.
fn write_boot_judgement(judgement: &BootJudgement) {
    let trend = judgement.trend;
    let (previous, current) = judgement.compared.counts().map_or((-1, -1), |(p, c)| {
        (
            i64::try_from(p).unwrap_or(i64::MAX),
            i64::try_from(c).unwrap_or(i64::MAX),
        )
    });
    info!(
        verdict = trend.reason(),
        compared = judgement.compared.reason(),
        previous_backlog_segments = previous,
        backlog_segments = current,
        record_skipped_planned = judgement.record_skipped_planned,
        "WAL replay backlog compared with the previous recorded start: replay limit lines \
         page unless it shrank (the planned evening restart is quiet and saves no count)"
    );
    if let Some(err) = &judgement.state_write_error {
        warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "wal_backlog_trend_state",
            error = %err,
            record_removed = judgement.record_removed,
            removed_record = judgement.removed_record.unwrap_or("none"),
            removal_synced = judgement.removal_synced,
            path = WAL_BACKLOG_TREND_STATE_PATH,
            "WAL backlog record could not be saved; this start pages its replay limit \
             lines, as does every later start that cannot save it (an unsynced removal \
             can bring a record back after a power cut)"
        );
    }
}

/// Writes this process's boot judgement once, after the log subscriber
/// exists (the judgement runs before it). Called by
/// `wal_unapplied_ranges::log_boot_fold_summary`. `false` when there is
/// nothing to write or it was already written. Cold.
#[must_use]
pub(crate) fn log_boot_backlog_trend() -> bool {
    let Some(judgement) = BOOT_JUDGEMENT.get() else {
        return false;
    };
    if BOOT_JUDGEMENT_LOGGED.swap(true, Ordering::AcqRel) {
        return false;
    }
    write_boot_judgement(judgement);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-11 IST.
    const TODAY: i64 = 20_768;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-backlog-trend-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn valid(day: i64, segments: Option<u64>) -> PreviousRecord {
        PreviousRecord::Valid(BacklogRecord {
            ist_day: day,
            segments,
        })
    }

    #[test]
    fn test_ist_day_of_unix_rolls_at_ist_midnight() {
        // 2026-11-11 00:00 IST = 2026-11-10 18:30 UTC = 1_794_335_400.
        assert_eq!(ist_day_of_unix(1_794_335_400), TODAY);
        assert_eq!(ist_day_of_unix(1_794_335_399), TODAY - 1);
        assert_eq!(ist_day_of_unix(0), 0);
        assert_eq!(ist_day_of_unix(-IST_OFFSET_SECS - 1), -1);
    }

    #[test]
    fn test_backlog_trend_verdict_every_permutation() {
        let v = backlog_trend_verdict;
        assert_eq!(
            v(PreviousRecord::Absent, TODAY, Some(5)),
            BacklogTrend::FirstStart
        );
        assert_eq!(
            v(PreviousRecord::Unreadable, TODAY, Some(5)),
            BacklogTrend::Unreadable
        );
        assert_eq!(
            v(PreviousRecord::Absent, TODAY, None),
            BacklogTrend::CurrentUnknown
        );
        assert_eq!(
            v(valid(TODAY, Some(9)), TODAY, None),
            BacklogTrend::CurrentUnknown
        );
        assert_eq!(
            v(valid(TODAY, None), TODAY, Some(3)),
            BacklogTrend::PreviousUnknown
        );
        assert_eq!(
            v(valid(TODAY + 1, Some(9)), TODAY, Some(3)),
            BacklogTrend::FutureDated
        );
        assert_eq!(
            v(valid(TODAY, Some(9)), TODAY, Some(3)),
            BacklogTrend::Shrinking {
                previous: 9,
                current: 3
            }
        );
        assert_eq!(
            v(valid(TODAY - 3, Some(1)), TODAY, Some(0)),
            BacklogTrend::Shrinking {
                previous: 1,
                current: 0
            },
            "an older record (a weekend) still compares; draining to 0 is shrinking"
        );
        assert_eq!(
            v(valid(TODAY, Some(9)), TODAY, Some(9)),
            BacklogTrend::Unchanged {
                previous: 9,
                current: 9
            }
        );
        assert_eq!(
            v(valid(TODAY, Some(0)), TODAY, Some(0)),
            BacklogTrend::Unchanged {
                previous: 0,
                current: 0
            }
        );
        assert_eq!(
            v(valid(TODAY, Some(9)), TODAY, Some(10)),
            BacklogTrend::Grew {
                previous: 9,
                current: 10
            }
        );
        // Only `Shrinking` quiets.
        for trend in [
            BacklogTrend::FirstStart,
            BacklogTrend::Unreadable,
            BacklogTrend::FutureDated,
            BacklogTrend::PreviousUnknown,
            BacklogTrend::CurrentUnknown,
            BacklogTrend::Unchanged {
                previous: 1,
                current: 1,
            },
            BacklogTrend::Grew {
                previous: 1,
                current: 2,
            },
        ] {
            assert!(!trend.is_shrinking(), "{trend:?}");
            assert!(trend.reason() != "shrinking");
        }
        assert!(
            BacklogTrend::Shrinking {
                previous: 2,
                current: 1
            }
            .is_shrinking()
        );
        assert_eq!(
            BacklogTrend::Grew {
                previous: 1,
                current: 2
            }
            .counts(),
            Some((1, 2))
        );
        assert_eq!(BacklogTrend::FirstStart.counts(), None);
    }

    /// Randomized: the verdict is `Shrinking` exactly when both counts are
    /// known, the record is not from the future, and the count fell.
    #[test]
    fn test_backlog_trend_verdict_shrinks_only_on_a_strict_known_fall() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let prev = (!x.is_multiple_of(7)).then_some(x % 50);
            let cur = (!(x >> 8).is_multiple_of(7)).then_some((x >> 16) % 50);
            let day = TODAY + i64::try_from((x >> 24) % 5).unwrap_or(0) - 2;
            let rec = match (x >> 32) % 4 {
                0 => PreviousRecord::Absent,
                1 => PreviousRecord::Unreadable,
                _ => valid(day, prev),
            };
            let expect = matches!(rec, PreviousRecord::Valid(r)
                if r.ist_day <= TODAY && matches!((r.segments, cur), (Some(p), Some(c)) if c < p));
            assert_eq!(
                backlog_trend_verdict(rec, TODAY, cur).is_shrinking(),
                expect,
                "{rec:?} {cur:?}"
            );
        }
    }

    #[test]
    fn test_backlog_record_to_line_and_parse_round_trip() {
        for segments in [Some(0), Some(285), Some(u64::MAX), None] {
            let rec = BacklogRecord {
                ist_day: TODAY,
                segments,
            };
            let line = rec.to_line().expect("in range");
            assert_eq!(BacklogRecord::parse(&line), Some(rec), "{line}");
        }
        assert_eq!(
            BacklogRecord {
                ist_day: TODAY,
                segments: Some(285)
            }
            .to_line()
            .as_deref(),
            Some(with_crc("v2 2026-11-11 285").as_str())
        );
        assert_eq!(
            BacklogRecord {
                ist_day: TODAY,
                segments: None
            }
            .to_line()
            .as_deref(),
            Some(with_crc("v2 2026-11-11 unknown").as_str())
        );
    }

    /// `body` plus its correct checksum word and newline.
    fn with_crc(body: &str) -> String {
        format!("{body} {:08x}\n", record_crc(body))
    }

    #[test]
    fn test_backlog_record_parse_refuses_every_corruption() {
        // Bodies refused even with a correct checksum.
        for bad in [
            "",
            "v1 2026-11-11 285",                  // old version
            "v3 2026-11-11 285",                  // wrong version
            "v2 2026-11-11",                      // no count
            "v2 2026-11-11 285 9",                // trailing word
            "v2 2026-11-11  285",                 // double space
            "v2 2026-13-11 285",                  // bad month
            "v2 2026-11-1 285",                   // short date
            "v2 2026-11-11 -1",                   // negative
            "v2 2026-11-11 +5",                   // sign
            "v2 2026-11-11 2.5",                  // fraction
            "v2 2026-11-11 99999999999999999999", // past u64
            "v2 2026-11-11 UNKNOWN",              // case
            "garbage",
        ] {
            let line = with_crc(bad);
            assert_eq!(BacklogRecord::parse(&line), None, "{line:?}");
        }
        let good = with_crc("v2 2026-11-11 285");
        assert!(BacklogRecord::parse(&good).is_some());
        let (body, crc) = good.trim_end().rsplit_once(' ').expect("crc word");
        for bad in [
            body.to_string() + "\n",                    // no checksum
            good.trim_end().to_string(),                // no newline
            good.clone() + "\n",                        // extra line
            format!("{body} {}\n", crc.to_uppercase()), // upper-case hex
            format!("{body} {}\n", &crc[1..]),          // short checksum
            format!("{body} 0{crc}\n"),                 // long checksum
            format!("{body} {crc} \n"),                 // trailing space
            format!("{body} zzzzzzzz\n"),               // not hex
        ] {
            assert_eq!(BacklogRecord::parse(&bad), None, "{bad:?}");
        }
    }

    /// Regression (LOW, 2026-11-11 review): a corrupted digit that still
    /// parses as a count must not read as a valid, higher previous count. The
    /// unchecked `v1` line `v1 2026-11-11 290` read back as `... 990` parsed,
    /// so the next start at 285 read `Shrinking` and stayed quiet on a stuck
    /// backlog. The checksum makes it unreadable, which pages.
    #[test]
    fn test_regression_a_flipped_digit_in_the_record_reads_unreadable_not_higher() {
        let line = BacklogRecord {
            ist_day: TODAY,
            segments: Some(290),
        }
        .to_line()
        .expect("in range");
        let flipped = line.replacen(" 290 ", " 990 ", 1);
        assert_ne!(line, flipped, "the digit was flipped");
        assert_eq!(BacklogRecord::parse(&flipped), None);
        let dir = scratch("flip");
        let path = dir.join("wal-backlog-trend.state");
        std::fs::write(&path, flipped.as_bytes()).expect("write");
        let previous = read_backlog_record(&path);
        assert_eq!(previous, PreviousRecord::Unreadable);
        assert!(!backlog_trend_verdict(previous, TODAY, Some(285)).is_shrinking());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_backlog_record_and_store_backlog_record() {
        let dir = scratch("rw");
        let path = dir.join("nested").join("wal-backlog-trend.state");
        assert_eq!(read_backlog_record(&path), PreviousRecord::Absent);
        let rec = BacklogRecord {
            ist_day: TODAY,
            segments: Some(42),
        };
        store_backlog_record(&path, rec).expect("store creates the folder");
        assert_eq!(read_backlog_record(&path), PreviousRecord::Valid(rec));
        assert!(!path.with_extension("tmp").exists(), "temp renamed away");
        std::fs::write(&path, b"not a record\n").expect("write");
        assert_eq!(read_backlog_record(&path), PreviousRecord::Unreadable);
        std::fs::write(&path, vec![b'1'; 4_096]).expect("write");
        assert_eq!(
            read_backlog_record(&path),
            PreviousRecord::Unreadable,
            "too long"
        );
        std::fs::remove_file(&path).expect("rm");
        std::fs::create_dir_all(&path).expect("a folder where the file belongs");
        assert_eq!(
            read_backlog_record(&path),
            PreviousRecord::Unreadable,
            "not a file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn one_segment_wal(tag: &str, segments: u64) -> std::path::PathBuf {
        let wal = scratch(tag);
        for i in 0..segments {
            // A file named like a segment but shorter than any record is
            // skipped by the scan; write real records instead.
            let seq = ((1_780_000_000 + 10_000 * i) * 1_000_000_000) >> 17 << 17;
            let ws = crate::ws_frame_spill::WsType::LiveFeed.as_u8();
            let ep = crate::ws_frame_spill::WalEndpoint::MainFeed.as_u8();
            let frame = [0xABu8; 32];
            let seq_le = seq.to_le_bytes();
            let recv = 7i64.to_le_bytes();
            let len = 32u32.to_le_bytes();
            let crc = crate::ws_frame_spill::crc32_ieee_of(&[
                &[ws],
                &seq_le[..],
                &recv[..],
                &[ep],
                &len[..],
                &frame[..],
            ]);
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"TVW4");
            bytes.push(ws);
            bytes.extend_from_slice(&seq_le);
            bytes.extend_from_slice(&recv);
            bytes.push(ep);
            bytes.extend_from_slice(&len);
            bytes.extend_from_slice(&frame);
            bytes.extend_from_slice(&crc.to_le_bytes());
            std::fs::write(wal.join(format!("ws-frames-{seq:020}.wal")), bytes).expect("segment");
        }
        // A trusted watermark with nothing acked: every waiting segment
        // counts, and the count is a known one.
        let wm = crate::wal_applied_watermark::AppliedWatermark::new();
        wm.bind(&wal);
        wm.persist_at(1);
        assert!(
            wal.join(crate::wal_applied_watermark::APPLIED_WATERMARK_FILE)
                .exists(),
            "watermark saved"
        );
        wal
    }

    /// Removes each test folder; the tests leave nothing in TMPDIR.
    fn clean(dirs: &[&std::path::Path]) {
        for dir in dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    /// The whole start, in a temp folder: first start pages and saves; a later
    /// start with fewer waiting segments is quiet; the same count pages.
    #[test]
    fn test_judge_at_boot_in_first_start_then_shrink_then_unchanged() {
        let state_dir = scratch("judge-state");
        let state = state_dir.join("wal-backlog-trend.state");
        let three = one_segment_wal("judge-three", 3);
        let first = judge_at_boot_in(&three, &state, TODAY, false);
        assert_eq!(first.trend, BacklogTrend::FirstStart);
        assert!(first.state_write_error.is_none());
        assert!(!first.record_skipped_planned);
        assert_eq!(
            read_backlog_record(&state),
            valid(TODAY, Some(3)),
            "the count is saved for the next start"
        );
        let two = one_segment_wal("judge-two", 2);
        let second = judge_at_boot_in(&two, &state, TODAY, false);
        assert_eq!(
            second.trend,
            BacklogTrend::Shrinking {
                previous: 3,
                current: 2
            }
        );
        let third = judge_at_boot_in(&two, &state, TODAY + 1, false);
        assert_eq!(
            third.trend,
            BacklogTrend::Unchanged {
                previous: 2,
                current: 2
            }
        );
        assert_eq!(read_backlog_record(&state), valid(TODAY + 1, Some(2)));
        clean(&[&state_dir, &three, &two]);
    }

    /// Regression (HIGH, 2026-11-11 review): the planned evening-drain
    /// restart saved its count, taken after the day's growth and before its
    /// own replay, so the next morning compared only against the evening's
    /// progress. A backlog growing ~5 segments a day (+30 in session, -25 in
    /// the evening) read `Shrinking` every morning and never paged. Now the
    /// planned restart saves nothing and the morning compares with the
    /// previous morning.
    #[test]
    fn test_regression_planned_evening_restart_does_not_hide_day_over_day_growth() {
        let state_dir = scratch("judge-planned");
        let state = state_dir.join("wal-backlog-trend.state");
        let (m1, e1, m2, e2, m3) = (
            one_segment_wal("pl-m1", 10),
            one_segment_wal("pl-e1", 15),
            one_segment_wal("pl-m2", 12),
            one_segment_wal("pl-e2", 16),
            one_segment_wal("pl-m3", 11),
        );
        // Day 1 morning: 10 waiting segments, recorded.
        let morning1 = judge_at_boot_in(&m1, &state, TODAY, false);
        assert_eq!(morning1.trend, BacklogTrend::FirstStart);
        // Day 1 evening (planned): the session added segments, 15 waiting.
        let evening = judge_at_boot_in(&e1, &state, TODAY, true);
        assert!(evening.record_skipped_planned);
        assert!(evening.state_write_error.is_none());
        assert_eq!(
            read_backlog_record(&state),
            valid(TODAY, Some(10)),
            "the planned restart leaves the morning's record"
        );
        // Day 2 morning: the evening drain retired some, 12 left: still more
        // than the previous morning, so it pages.
        let morning2 = judge_at_boot_in(&m2, &state, TODAY + 1, false);
        assert_eq!(
            morning2.trend,
            BacklogTrend::Grew {
                previous: 10,
                current: 12
            },
            "day-over-day growth pages"
        );
        // Day 2 evening (planned) then day 3 morning below day 2's: quiet.
        let _ = judge_at_boot_in(&e2, &state, TODAY + 1, true);
        let morning3 = judge_at_boot_in(&m3, &state, TODAY + 2, false);
        assert!(morning3.trend.is_shrinking(), "{:?}", morning3.trend);
        clean(&[&state_dir, &m1, &e1, &m2, &e2, &m3]);
    }

    /// An unlistable WAL folder pages and saves `unknown`, so the next start
    /// pages too.
    #[test]
    fn test_judge_at_boot_in_unknown_count_saves_unknown() {
        let state_dir = scratch("judge-unknown");
        let state = state_dir.join("wal-backlog-trend.state");
        let wal = scratch("judge-unlistable");
        std::fs::write(wal.join(crate::ws_frame_spill::REPLAYING_SUBDIR), b"x")
            .expect("a file where a folder belongs");
        let j = judge_at_boot_in(&wal, &state, TODAY, false);
        assert_eq!(j.trend, BacklogTrend::CurrentUnknown);
        assert_eq!(read_backlog_record(&state), valid(TODAY, None));
        let after = one_segment_wal("judge-after-unknown", 1);
        let next = judge_at_boot_in(&after, &state, TODAY, false);
        assert_eq!(next.trend, BacklogTrend::PreviousUnknown);
        clean(&[&state_dir, &wal, &after]);
    }

    /// Regression (LOW, 2026-11-11 review): a start whose applied watermark
    /// was rejected counted every waiting segment (applied live segments
    /// waiting only for the S3 copy included) and saved that inflated number
    /// as a known count. The next start, with a clean watermark, read
    /// `Shrinking` against it and stayed quiet on a stuck backlog. Now the
    /// untrusted start saves `unknown`, so the next one pages.
    #[test]
    fn test_regression_an_untrusted_watermark_saves_unknown_not_an_inflated_count() {
        let state_dir = scratch("judge-untrusted");
        let state = state_dir.join("wal-backlog-trend.state");
        // Start A (clean): the stuck backlog, here 2 segments
        // still waiting after 1 applied one (acked below).
        let wal = one_segment_wal("judge-untrusted-wal", 3);
        let wm = crate::wal_applied_watermark::AppliedWatermark::new();
        wm.bind(&wal);
        // Ack halfway to the second segment (segments sit 10,000 s apart):
        // the first segment is applied, the other two are not.
        let ack = ((1_780_000_000u64 + 5_000) * 1_000_000_000) >> 17 << 17;
        wm.note_ticks_acked(ack);
        wm.note_depth_acked(ack);
        let lag = crate::wal_applied_watermark::WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000;
        wm.persist_at(1);
        wm.persist_at(1 + lag + 1);
        let a = judge_at_boot_in(&wal, &state, TODAY, false);
        assert_eq!(a.trend, BacklogTrend::FirstStart);
        assert_eq!(read_backlog_record(&state), valid(TODAY, Some(2)));

        // Start B: the watermark file is rejected (as after a clock step back
        // or a crash between its two writes).
        let watermark = wal.join(crate::wal_applied_watermark::APPLIED_WATERMARK_FILE);
        let good = std::fs::read(&watermark).expect("read watermark");
        std::fs::write(&watermark, b"rejected").expect("corrupt");
        let b = judge_at_boot_in(&wal, &state, TODAY, false);
        assert_eq!(b.trend, BacklogTrend::CurrentUnknown, "B pages");
        assert_eq!(
            read_backlog_record(&state),
            valid(TODAY, None),
            "B saves unknown, never 3"
        );

        // Start C: the watermark loads cleanly again, nothing drained.
        std::fs::write(&watermark, good).expect("restore");
        let c = judge_at_boot_in(&wal, &state, TODAY + 1, false);
        assert_eq!(c.trend, BacklogTrend::PreviousUnknown, "C pages too");
        assert!(!c.trend.is_shrinking());
        clean(&[&state_dir, &wal]);
    }

    #[test]
    fn test_trend_after_save_pages_on_a_failed_save() {
        let shrinking = BacklogTrend::Shrinking {
            previous: 9,
            current: 1,
        };
        assert_eq!(trend_after_save(shrinking, true), shrinking);
        assert_eq!(
            trend_after_save(shrinking, false),
            BacklogTrend::StateNotSaved
        );
        assert_eq!(
            trend_after_save(BacklogTrend::FirstStart, false),
            BacklogTrend::StateNotSaved
        );
        assert!(!BacklogTrend::StateNotSaved.is_shrinking());
        assert_eq!(BacklogTrend::StateNotSaved.reason(), "state_not_saved");
        assert_eq!(BacklogTrend::StateNotSaved.counts(), None);
    }

    /// A save that fails pages this start and removes the old record, so the
    /// next start cannot compare against a stale, higher count either.
    #[test]
    fn test_judge_at_boot_in_failed_save_removes_the_old_record() {
        let dir = scratch("judge-fail");
        let state = dir.join("wal-backlog-trend.state");
        store_backlog_record(
            &state,
            BacklogRecord {
                ist_day: TODAY,
                segments: Some(9),
            },
        )
        .expect("seed");
        // A folder where the temp file goes makes `File::create` fail.
        std::fs::create_dir_all(state.with_extension("tmp")).expect("block the temp file");
        let wal = one_segment_wal("judge-fail-wal", 1);
        let j = judge_at_boot_in(&wal, &state, TODAY, false);
        assert_eq!(j.trend, BacklogTrend::StateNotSaved);
        assert_eq!(
            j.compared,
            BacklogTrend::Shrinking {
                previous: 9,
                current: 1
            }
        );
        assert!(j.state_write_error.is_some());
        assert!(j.record_removed);
        assert_eq!(j.removed_record, Some("previous_start"));
        assert!(j.removal_synced);
        assert_eq!(
            read_backlog_record(&state),
            PreviousRecord::Absent,
            "next start pages"
        );
        clean(&[&dir, &wal]);
    }

    /// Regression (LOW, 2026-11-11 review): when only the folder sync after
    /// the rename failed, the record removed was THIS start's, but the log
    /// said `old_record_removed = true`. The judgement now says which record
    /// went and whether the removal was synced.
    #[test]
    fn test_regression_a_failed_folder_sync_reports_this_starts_record_removed() {
        let dir = scratch("judge-sync");
        let state = dir.join("wal-backlog-trend.state");
        store_backlog_record(
            &state,
            BacklogRecord {
                ist_day: TODAY,
                segments: Some(9),
            },
        )
        .expect("seed");
        let wal = one_segment_wal("judge-sync-wal", 1);
        let failing = |_: &Path| -> std::io::Result<()> { Err(std::io::Error::other("EIO")) };
        let j = judge_at_boot_in_with(&wal, &state, TODAY, false, failing);
        assert_eq!(j.trend, BacklogTrend::StateNotSaved);
        assert_eq!(j.state_write_error.as_deref(), Some("EIO"));
        assert!(j.record_removed);
        assert_eq!(j.removed_record, Some("this_start"), "the new record went");
        assert!(!j.removal_synced, "and its removal was not synced");
        assert_eq!(read_backlog_record(&state), PreviousRecord::Absent);

        // The store itself reports the rename as landed.
        let failure = store_backlog_record_with(
            &state,
            BacklogRecord {
                ist_day: TODAY,
                segments: Some(1),
            },
            failing,
        )
        .expect_err("sync fails");
        assert!(failure.renamed);
        assert_eq!(read_backlog_record(&state), valid(TODAY, Some(1)));
        clean(&[&dir, &wal]);
    }

    #[test]
    fn test_removed_record_label_names_the_record() {
        assert_eq!(removed_record_label(true), "this_start");
        assert_eq!(removed_record_label(false), "previous_start");
    }

    /// Regression (MEDIUM, 2026-11-11 review): with `data/state` unwritable
    /// the old record cannot be removed either (unlink needs the same write
    /// permission), so a stale higher count stays and every later start read
    /// `Shrinking` against it, quiet forever on a stuck backlog. The tests
    /// run as root, which can always unlink, so the left-behind record is
    /// put back by hand after each failed start; what matters is that every
    /// start that cannot save pages whatever it compared.
    #[test]
    fn test_regression_an_unwritable_state_folder_pages_on_every_start() {
        let dir = scratch("judge-frozen");
        let state = dir.join("wal-backlog-trend.state");
        let frozen = BacklogRecord {
            ist_day: TODAY - 1,
            segments: Some(300),
        };
        std::fs::create_dir_all(state.with_extension("tmp")).expect("block every save");
        let stuck = one_segment_wal("judge-frozen-wal", 2);
        for day in 0..3 {
            std::fs::write(&state, frozen.to_line().expect("in range")).expect("frozen record");
            let j = judge_at_boot_in(&stuck, &state, TODAY + day, false);
            assert!(j.compared.is_shrinking(), "the comparison alone is fooled");
            assert_eq!(j.trend, BacklogTrend::StateNotSaved, "day {day} pages");
            assert!(j.state_write_error.is_some());
        }
        clean(&[&dir, &stuck]);
    }

    /// `boot_backlog_trend` is `None` until `judge_at_boot` runs, which no
    /// test in this binary calls; `log_boot_backlog_trend` then writes
    /// nothing.
    #[test]
    fn test_boot_backlog_trend_and_log_boot_backlog_trend_before_any_judgement() {
        assert_eq!(boot_backlog_trend(), None);
        assert!(!log_boot_backlog_trend());
        // The lines themselves, for a failed save.
        write_boot_judgement(&BootJudgement {
            trend: BacklogTrend::StateNotSaved,
            compared: BacklogTrend::Grew {
                previous: 1,
                current: 2,
            },
            record_skipped_planned: false,
            state_write_error: Some("disk full".to_string()),
            record_removed: true,
            removed_record: Some(removed_record_label(false)),
            removal_synced: true,
        });
    }
}
