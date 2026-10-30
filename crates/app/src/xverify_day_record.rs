//! The day's subscribed main-feed spot set, for the 1-minute cross-check
//! (plan item 51e, `no-rest-except-live-feed-2026-06-27.md` §12.15.13).
//!
//! # Why
//!
//! The cross-check used to verify the set the process booted with. A session
//! that booted on the four index ids and widened to today's list later checked
//! only the four indices, and a restart checked only what the new process
//! subscribed. This file is the day's own record: every main-feed spot set the
//! day subscribed, from any process, with the EARLIEST second of the IST day it
//! was subscribed. The check reads it when it fires, so its targets are the
//! day's, not the boot's, and it can say how much of the session the targets
//! were subscribed for.
//!
//! # Shape
//!
//! `data/instrument-cache/main-feed-spot-<YYYY-MM-DD>.json`, one file per IST
//! day, kept [`DAY_RECORD_KEEP_DAYS`] days. Each recording MIN-merges: a key
//! already present keeps the earlier of its two seconds, so a restart never
//! moves an instrument's first second later. Two day flags are OR-merged:
//! `master_listed` (some recording came from a day list) and `index_fallback`
//! (some recording was the four index ids while master sourcing was asked
//! for). A day whose only recordings are the index fallback is the four-index
//! fallback day (plan item 51f).
//!
//! # Cost
//!
//! Cold path only: one read, one merge and at most one write per recording
//! (the daily recorder, and the end of a widen): **O(file bytes)** to parse,
//! O(instruments) to merge (one hash probe each), **O(n log n)** to write (the
//! entries are sorted so the file is stable), plus an O(files) sweep of old day
//! files after a write. Never per tick.
//! The write is a temp file, `sync_all`, `rename` and a folder `sync_all`, run
//! through `off_worker` so a slow disk never holds a shared runtime worker.
//! SPACE: at most [`DAY_RECORD_MAX_INSTRUMENTS`] keys (the main-feed capacity);
//! a recording past it is refused for the new keys and counted in the result.
//!
//! # Honest limits
//!
//! - The boot recording is stamped at the resolve, a few seconds BEFORE the
//!   sockets confirm the subscribe; the widen recording is stamped when the
//!   widen finishes, AFTER its subscribes. Coverage is counted in whole minutes
//!   that start at or after the recorded second.
//! - The record says when an instrument was FIRST subscribed today. It does not
//!   see a later gap (a process down mid-session), which the tick conservation
//!   check and the WAL cover.
//! - A widen that finished short (no room, or a connection that did not open)
//!   records today's whole list: which instruments landed on the missing room
//!   is not known where the widen finishes. The unsubscribed ones then show as
//!   missing minutes (a louder finding, never a quieter one).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{Datelike, NaiveDate};
use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Folder of the day records (beside `depth-held-today.json`).
pub const DAY_RECORD_DIR: &str = "data/instrument-cache";

/// File name prefix of one day's record.
const DAY_RECORD_PREFIX: &str = "main-feed-spot-";

/// File name suffix of one day's record.
const DAY_RECORD_SUFFIX: &str = ".json";

/// Suffix of the temp file a write renames into place.
const DAY_RECORD_TMP_SUFFIX: &str = ".json.tmp";

/// Days a record is kept. Above the S3 hold ceiling
/// (`partition_archive::MAX_CROSSVERIFY_HOLD_DAYS`, 3) plus a long weekend, so
/// the past-day catch-up (plan item 51g) can still read every day it may run.
pub const DAY_RECORD_KEEP_DAYS: i64 = 10;

const _: () = assert!(
    DAY_RECORD_KEEP_DAYS > tickvault_storage::partition_archive::MAX_CROSSVERIFY_HOLD_DAYS + 3,
    "a day record must outlive every day the catch-up may still check"
);

/// The most keys one day record holds: the main-feed subscription capacity.
pub const DAY_RECORD_MAX_INSTRUMENTS: usize = 25_000;

/// The largest file the reader accepts. One entry is under 80 bytes of JSON,
/// so 25,000 entries fit with room; a bigger file is refused, never parsed.
const DAY_RECORD_READ_CAP_BYTES: u64 = 4 * 1024 * 1024;

/// Serialises read-merge-write so two recordings in one process (the boot and
/// a widen) never lose each other's keys.
static RECORD_LOCK: Mutex<()> = Mutex::new(());

/// The main-feed spot sets this process holds on its sockets, one per origin
/// (the boot set, and the widened day list once a widen finishes), for the
/// daily recorder (§12.15.13, review round 1). At most two entries, cold.
static PROCESS_HELD: Mutex<Vec<HeldSet>> = Mutex::new(Vec::new());

/// One noted set: where it came from in this process, the set, and its basis.
pub type HeldSet = (HeldOrigin, Vec<SubscribeInstrument>, UniverseBasis);

/// Which step of this process put a set on the main-feed sockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldOrigin {
    /// The boot subscribe.
    Boot,
    /// A finished widen to today's list (added beside the boot set).
    Widen,
}

/// Notes a set this process holds on its main-feed sockets, replacing an
/// earlier set of the same origin. The daily recorder writes every noted set
/// into each IST day's record from 08:00 until the close, so a process alive
/// across midnight records the new day too. O(entries ≤ 2) plus one copy.
pub fn note_process_held(
    origin: HeldOrigin,
    instruments: &[SubscribeInstrument],
    basis: UniverseBasis,
) {
    let mut held = PROCESS_HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // O(1) EXEMPT: at most one entry per origin, two origins.
    held.retain(|(o, _, _)| *o != origin);
    held.push((origin, instruments.to_vec(), basis));
}

/// Every set noted by [`note_process_held`], cloned. O(total instruments),
/// once a day, cold.
#[must_use]
pub fn process_held() -> Vec<HeldSet> {
    PROCESS_HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The IST day (days from CE, 0 = none) on which a day-record write failed
/// in this process and no later write of every held set succeeded (review
/// round 2). The check reads it as coverage unknown: the record may lack a
/// set that is on the wire.
static WRITE_FAILED_DAY: AtomicI64 = AtomicI64::new(0);

fn day_number(date_ist: NaiveDate) -> i64 {
    i64::from(date_ist.num_days_from_ce())
}

/// Notes that a write into `date_ist`'s record failed. One store, O(1).
pub fn note_record_write_failed(date_ist: NaiveDate) {
    WRITE_FAILED_DAY.store(day_number(date_ist), Ordering::Release);
}

/// Clears a failure noted for `date_ist` once every held set was written
/// again. One compare-and-swap, O(1); a failure noted for another day stays.
pub fn clear_record_write_failed(date_ist: NaiveDate) {
    let _was = WRITE_FAILED_DAY.compare_exchange(
        day_number(date_ist),
        0,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

/// Whether a write into `date_ist`'s record failed and was not yet redone.
/// One load, O(1).
#[must_use]
pub fn record_write_failed_for(date_ist: NaiveDate) -> bool {
    WRITE_FAILED_DAY.load(Ordering::Acquire) == day_number(date_ist)
}

/// Where one recording's set came from (plan item 51f).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniverseBasis {
    /// A day list from the daily rider (today's, an earlier day's, or filled
    /// to capacity).
    MasterList,
    /// The four index ids because master sourcing is switched OFF in config:
    /// the intended universe, not a fallback.
    ConfiguredIndices,
    /// The four index ids although master sourcing was asked for: the list was
    /// missing or unusable.
    IndexFallback,
}

impl UniverseBasis {
    /// Stable label for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MasterList => "master_list",
            Self::ConfiguredIndices => "configured_indices",
            Self::IndexFallback => "index_fallback",
        }
    }
}

/// The basis of the boot set: the four index ids exactly (every instrument in
/// `index_universe`, nothing else) is the index universe; with master sourcing
/// on that is the fallback. Pure, O(set × index ids), at most 4 index ids.
#[must_use]
pub fn universe_basis(
    from_master: bool,
    set: &[SubscribeInstrument],
    index_universe: &[SubscribeInstrument],
) -> UniverseBasis {
    // O(1) EXEMPT: the index universe is the 4 hardcoded index ids.
    let only_indices = set.iter().all(|i| index_universe.contains(i));
    match (from_master, only_indices) {
        (false, _) => UniverseBasis::ConfiguredIndices,
        (true, true) => UniverseBasis::IndexFallback,
        (true, false) => UniverseBasis::MasterList,
    }
}

/// One day's record, in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotDayRecord {
    /// The IST day it describes.
    pub date_ist: NaiveDate,
    /// Some recording of the day came from a day list.
    pub master_listed: bool,
    /// Some recording of the day was the four-index fallback.
    pub index_fallback: bool,
    /// Composite key `(security_id, segment byte)` (I-P1-11) to the earliest
    /// second of the IST day it was subscribed.
    first_secs: HashMap<(SecurityId, u8), u32>,
}

/// What one merge changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Keys not in the record before.
    pub added: usize,
    /// Keys whose first second moved earlier.
    pub moved_earlier: usize,
    /// New keys refused because the record was full.
    pub refused_full: usize,
    /// A day flag changed.
    pub flags_changed: bool,
}

impl MergeOutcome {
    /// Whether the record changed, so it needs writing.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.added > 0 || self.moved_earlier > 0 || self.flags_changed
    }
}

impl SpotDayRecord {
    /// An empty record for `date_ist`.
    #[must_use]
    pub fn new(date_ist: NaiveDate) -> Self {
        Self {
            date_ist,
            master_listed: false,
            index_fallback: false,
            first_secs: HashMap::new(),
        }
    }

    /// Min-merges one recording: every instrument keeps the earlier of its
    /// recorded second and `secs_of_day`; the day flags are OR-ed with
    /// `basis`. A new key past [`DAY_RECORD_MAX_INSTRUMENTS`] is refused.
    /// O(instruments), one hash probe each.
    pub fn merge(
        &mut self,
        instruments: &[SubscribeInstrument],
        secs_of_day: u32,
        basis: UniverseBasis,
    ) -> MergeOutcome {
        let mut out = MergeOutcome::default();
        match basis {
            UniverseBasis::MasterList if !self.master_listed => {
                self.master_listed = true;
                out.flags_changed = true;
            }
            UniverseBasis::IndexFallback if !self.index_fallback => {
                self.index_fallback = true;
                out.flags_changed = true;
            }
            UniverseBasis::MasterList
            | UniverseBasis::IndexFallback
            | UniverseBasis::ConfiguredIndices => {}
        }
        for instrument in instruments {
            let key = (instrument.security_id, instrument.segment.binary_code());
            let full = self.first_secs.len() >= DAY_RECORD_MAX_INSTRUMENTS;
            match self.first_secs.get_mut(&key) {
                Some(first) => {
                    if secs_of_day < *first {
                        *first = secs_of_day;
                        out.moved_earlier = out.moved_earlier.saturating_add(1);
                    }
                }
                None if full => {
                    out.refused_full = out.refused_full.saturating_add(1);
                }
                None => {
                    self.first_secs.insert(key, secs_of_day);
                    out.added = out.added.saturating_add(1);
                }
            }
        }
        out
    }

    /// The four-index fallback day: some recording was the fallback and none
    /// came from a day list (plan item 51f). O(1).
    #[must_use]
    pub const fn is_index_fallback_day(&self) -> bool {
        self.index_fallback && !self.master_listed
    }

    /// Number of instruments recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.first_secs.len()
    }

    /// Whether nothing is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.first_secs.is_empty()
    }

    /// The earliest recorded second for an instrument, by its composite key.
    #[must_use]
    pub fn first_secs_of_day(&self, instrument: &SubscribeInstrument) -> Option<u32> {
        self.first_secs
            .get(&(instrument.security_id, instrument.segment.binary_code()))
            .copied()
    }

    /// Every recorded instrument with its first second, sorted by
    /// `(segment, security_id)` so a run's target order never depends on hash
    /// order. A key whose segment byte no longer decodes is skipped (it was
    /// written by this module, so this is unreachable short of a hand edit).
    /// O(n log n), cold.
    #[must_use]
    pub fn instruments(&self) -> Vec<(SubscribeInstrument, u32)> {
        let mut keys: Vec<(&(SecurityId, u8), &u32)> = self.first_secs.iter().collect();
        keys.sort_unstable_by_key(|((id, seg), _)| (*seg, *id));
        keys.into_iter()
            .filter_map(|((id, seg), first)| {
                ExchangeSegment::from_byte(*seg).map(|segment| {
                    (
                        SubscribeInstrument {
                            security_id: *id,
                            segment,
                        },
                        *first,
                    )
                })
            })
            .collect()
    }
}

/// On-disk form.
#[derive(serde::Serialize, serde::Deserialize)]
struct DayRecordFile {
    date_ist: String,
    master_listed: bool,
    index_fallback: bool,
    instruments: Vec<DayRecordEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DayRecordEntry {
    security_id: SecurityId,
    segment: u8,
    first_secs_of_day: u32,
}

/// `<base>/main-feed-spot-<date>.json`.
fn record_path_in(base: &Path, date_ist: NaiveDate) -> PathBuf {
    base.join(format!("{DAY_RECORD_PREFIX}{date_ist}{DAY_RECORD_SUFFIX}"))
}

/// The record path for `date_ist` under [`DAY_RECORD_DIR`].
#[must_use]
pub fn day_record_path(date_ist: NaiveDate) -> PathBuf {
    record_path_in(Path::new(DAY_RECORD_DIR), date_ist)
}

/// Reads one day's record. `Ok(None)` when the file does not exist; `Err`
/// when it cannot be read, is over [`DAY_RECORD_READ_CAP_BYTES`], does not
/// parse, names another day, or holds a key twice. O(file bytes), cold.
fn read_record_in(base: &Path, date_ist: NaiveDate) -> std::io::Result<Option<SpotDayRecord>> {
    let path = record_path_in(base, date_ist);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut bytes = Vec::new();
    file.take(DAY_RECORD_READ_CAP_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > DAY_RECORD_READ_CAP_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "day record over the read cap",
        ));
    }
    let parsed: DayRecordFile = serde_json::from_slice(&bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if parsed.date_ist != date_ist.to_string() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "day record names another day",
        ));
    }
    let mut record = SpotDayRecord::new(date_ist);
    record.master_listed = parsed.master_listed;
    record.index_fallback = parsed.index_fallback;
    for entry in parsed.instruments {
        if record.first_secs.len() >= DAY_RECORD_MAX_INSTRUMENTS {
            break;
        }
        if record
            .first_secs
            .insert((entry.security_id, entry.segment), entry.first_secs_of_day)
            .is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "day record holds a key twice",
            ));
        }
    }
    Ok(Some(record))
}

/// Writes `record` atomically: temp file, `sync_all`, `rename`, folder
/// `sync_all`; then removes record files older than
/// [`DAY_RECORD_KEEP_DAYS`]. A folder sync failure after the rename is not an
/// error (the file is in place). O(n log n) (sorted entries) + O(files), cold.
fn write_record_in(base: &Path, record: &SpotDayRecord) -> std::io::Result<()> {
    let mut instruments: Vec<DayRecordEntry> = record
        .first_secs
        .iter()
        .map(|((security_id, segment), first)| DayRecordEntry {
            security_id: *security_id,
            segment: *segment,
            first_secs_of_day: *first,
        })
        .collect();
    instruments.sort_unstable_by_key(|e| (e.segment, e.security_id));
    let file = DayRecordFile {
        date_ist: record.date_ist.to_string(),
        master_listed: record.master_listed,
        index_fallback: record.index_fallback,
        instruments,
    };
    let bytes = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(base)?;
    let path = record_path_in(base, record.date_ist);
    let tmp = base.join(format!(
        "{DAY_RECORD_PREFIX}{}{DAY_RECORD_TMP_SUFFIX}",
        record.date_ist
    ));
    let written = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(&bytes)?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        // Best-effort: a leftover temp file is overwritten by the next write.
        let _removed = std::fs::remove_file(&tmp);
        return Err(err);
    }
    // Folder sync: best-effort, the rename already happened.
    let _synced = std::fs::File::open(base).and_then(|dir| dir.sync_all());
    sweep_old_records_in(base, record.date_ist);
    Ok(())
}

/// Removes record files (and their temp files) dated more than
/// [`DAY_RECORD_KEEP_DAYS`] before `date_ist`. Foreign names are left alone.
/// O(files in `base`).
fn sweep_old_records_in(base: &Path, date_ist: NaiveDate) {
    let Some(cutoff) = chrono::TimeDelta::try_days(DAY_RECORD_KEEP_DAYS)
        .and_then(|keep| date_ist.checked_sub_signed(keep))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(DAY_RECORD_PREFIX) else {
            continue;
        };
        let Some(stem) = rest
            .strip_suffix(DAY_RECORD_TMP_SUFFIX)
            .or_else(|| rest.strip_suffix(DAY_RECORD_SUFFIX))
        else {
            continue;
        };
        let Ok(day) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
            continue;
        };
        if day < cutoff {
            let _removed = std::fs::remove_file(entry.path());
        }
    }
}

/// What a recording did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordOutcome {
    /// What the merge changed.
    pub merge: MergeOutcome,
    /// The file on disk was unreadable and was replaced by this recording
    /// alone (earlier recordings of the day are lost; the next ones re-add).
    pub replaced_unreadable: bool,
    /// Instruments in the record after this recording.
    pub recorded: usize,
}

/// Records `instruments` as subscribed at `secs_of_day` on `date_ist`, under
/// `base`. Read, merge, and write only when something changed. A file that
/// cannot be read is replaced (reported in the outcome). O(file bytes +
/// instruments + n log n), cold.
fn record_in(
    base: &Path,
    date_ist: NaiveDate,
    instruments: &[SubscribeInstrument],
    secs_of_day: u32,
    basis: UniverseBasis,
) -> std::io::Result<RecordOutcome> {
    let _guard = RECORD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut record, replaced_unreadable) = match read_record_in(base, date_ist) {
        Ok(Some(record)) => (record, false),
        Ok(None) => (SpotDayRecord::new(date_ist), false),
        Err(_) => (SpotDayRecord::new(date_ist), true),
    };
    let merge = record.merge(instruments, secs_of_day, basis);
    if merge.changed() || replaced_unreadable {
        write_record_in(base, &record)?;
    }
    Ok(RecordOutcome {
        merge,
        replaced_unreadable,
        recorded: record.len(),
    })
}

/// Records `instruments` as subscribed on the main feed at `secs_of_day` of
/// `date_ist` (plan item 51e). Runs through `off_worker`, so a slow disk never
/// holds a shared runtime worker.
///
/// # Errors
/// The record could not be written; the caller logs it with its own code.
pub fn record_spot_subscribed(
    date_ist: NaiveDate,
    instruments: &[SubscribeInstrument],
    secs_of_day: u32,
    basis: UniverseBasis,
) -> std::io::Result<RecordOutcome> {
    tickvault_storage::off_worker::off_worker(|| {
        record_in(
            Path::new(DAY_RECORD_DIR),
            date_ist,
            instruments,
            secs_of_day,
            basis,
        )
    })
}

/// Reads `date_ist`'s record from [`DAY_RECORD_DIR`] (plan item 51e).
/// `Ok(None)` when no process recorded that day. Runs through `off_worker`.
///
/// # Errors
/// The file exists but cannot be trusted (unreadable, over the cap, not
/// parseable, another day's, or a key twice).
pub fn read_spot_day_record(date_ist: NaiveDate) -> std::io::Result<Option<SpotDayRecord>> {
    tickvault_storage::off_worker::off_worker(|| {
        read_record_in(Path::new(DAY_RECORD_DIR), date_ist)
    })
}

/// The current IST second of the day, from the wall clock. O(1).
#[must_use]
pub fn ist_secs_of_day_now() -> u32 {
    let ist = chrono::Utc::now().with_timezone(&tickvault_common::trading_calendar::ist_offset());
    chrono::Timelike::num_seconds_from_midnight(&ist.time())
}

/// The current IST date, from the wall clock. O(1).
#[must_use]
pub fn ist_date_now() -> NaiveDate {
    chrono::Utc::now()
        .with_timezone(&tickvault_common::trading_calendar::ist_offset())
        .date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _removed = std::fs::remove_dir_all(&self.0);
        }
    }

    static TMP_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn unique_dir() -> TmpDir {
        let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tv_xverify_day_record_{}_{}",
            std::process::id(),
            n
        ));
        let _created = std::fs::create_dir_all(&dir);
        TmpDir(dir)
    }

    fn inst(id: SecurityId, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 30).unwrap_or_default()
    }

    fn indices() -> Vec<SubscribeInstrument> {
        vec![
            inst(13, ExchangeSegment::IdxI),
            inst(25, ExchangeSegment::IdxI),
            inst(51, ExchangeSegment::IdxI),
            inst(21, ExchangeSegment::IdxI),
        ]
    }

    #[test]
    fn test_universe_basis_tells_the_fallback_from_the_configured_indices() {
        let idx = indices();
        assert_eq!(
            universe_basis(false, &idx, &idx),
            UniverseBasis::ConfiguredIndices
        );
        assert_eq!(
            universe_basis(true, &idx, &idx),
            UniverseBasis::IndexFallback
        );
        let mut wide = idx.clone();
        wide.push(inst(2885, ExchangeSegment::NseEquity));
        assert_eq!(universe_basis(true, &wide, &idx), UniverseBasis::MasterList);
        // The same id in another segment is not an index (I-P1-11).
        let collide = vec![inst(13, ExchangeSegment::NseEquity)];
        assert_eq!(
            universe_basis(true, &collide, &idx),
            UniverseBasis::MasterList
        );
        assert_eq!(UniverseBasis::IndexFallback.as_str(), "index_fallback");
        assert_eq!(UniverseBasis::MasterList.as_str(), "master_list");
        assert_eq!(
            UniverseBasis::ConfiguredIndices.as_str(),
            "configured_indices"
        );
    }

    #[test]
    fn test_merge_keeps_the_earlier_second_so_a_restart_never_moves_it_later() {
        let mut r = SpotDayRecord::new(day());
        let a = inst(2885, ExchangeSegment::NseEquity);
        let out = r.merge(&[a], 30_000, UniverseBasis::MasterList);
        assert_eq!(out.added, 1);
        assert!(out.changed());
        // A later process records the same instrument later: unchanged.
        let out = r.merge(&[a], 40_000, UniverseBasis::MasterList);
        assert!(!out.changed());
        assert_eq!(r.first_secs_of_day(&a), Some(30_000));
        // An earlier recording moves it earlier.
        let out = r.merge(&[a], 29_000, UniverseBasis::MasterList);
        assert_eq!(out.moved_earlier, 1);
        assert_eq!(r.first_secs_of_day(&a), Some(29_000));
    }

    #[test]
    fn test_first_secs_of_day_keys_on_the_composite_identity() {
        let mut r = SpotDayRecord::new(day());
        let idx = inst(27, ExchangeSegment::IdxI);
        let eq = inst(27, ExchangeSegment::NseEquity);
        r.merge(&[idx], 100, UniverseBasis::MasterList);
        r.merge(&[eq], 200, UniverseBasis::MasterList);
        assert_eq!(r.len(), 2);
        assert_eq!(r.first_secs_of_day(&idx), Some(100));
        assert_eq!(r.first_secs_of_day(&eq), Some(200));
    }

    #[test]
    fn test_is_index_fallback_day_needs_a_fallback_and_no_master_list() {
        let mut r = SpotDayRecord::new(day());
        assert!(!r.is_index_fallback_day());
        r.merge(&indices(), 15_000, UniverseBasis::IndexFallback);
        assert!(r.is_index_fallback_day());
        // A later widen to today's list clears it.
        r.merge(
            &[inst(2885, ExchangeSegment::NseEquity)],
            29_000,
            UniverseBasis::MasterList,
        );
        assert!(!r.is_index_fallback_day());
        // Configured indices alone are never a fallback day.
        let mut c = SpotDayRecord::new(day());
        c.merge(&indices(), 30_000, UniverseBasis::ConfiguredIndices);
        assert!(!c.is_index_fallback_day());
    }

    #[test]
    fn test_merge_refuses_new_keys_past_the_cap_and_keeps_tracked_ones() {
        let mut r = SpotDayRecord::new(day());
        let many: Vec<SubscribeInstrument> = (1..=DAY_RECORD_MAX_INSTRUMENTS as u64)
            .map(|id| inst(id, ExchangeSegment::NseEquity))
            .collect();
        let out = r.merge(&many, 30_000, UniverseBasis::MasterList);
        assert_eq!(out.added, DAY_RECORD_MAX_INSTRUMENTS);
        let extra = inst(9_999_999, ExchangeSegment::NseEquity);
        let out = r.merge(&[extra, many[0]], 1, UniverseBasis::MasterList);
        assert_eq!(out.refused_full, 1);
        assert_eq!(out.moved_earlier, 1, "a tracked key still moves earlier");
        assert_eq!(r.len(), DAY_RECORD_MAX_INSTRUMENTS);
    }

    #[test]
    fn test_record_round_trips_and_min_merges_across_processes() {
        let dir = unique_dir();
        let a = inst(2885, ExchangeSegment::NseEquity);
        let b = inst(13, ExchangeSegment::IdxI);
        let out = record_in(
            dir.path(),
            day(),
            &[a, b],
            30_000,
            UniverseBasis::MasterList,
        )
        .unwrap_or_else(|e| panic!("record: {e}"));
        assert_eq!(out.recorded, 2);
        // A restart later in the day records the same set later: the earlier
        // second stays.
        let out = record_in(dir.path(), day(), &[a], 45_000, UniverseBasis::MasterList)
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(!out.merge.changed());
        let read = read_record_in(dir.path(), day())
            .unwrap_or_else(|e| panic!("read: {e}"))
            .unwrap_or_else(|| panic!("record missing"));
        assert_eq!(read.first_secs_of_day(&a), Some(30_000));
        assert_eq!(read.first_secs_of_day(&b), Some(30_000));
        assert!(read.master_listed);
        assert!(!read.index_fallback);
        // Sorted by (segment, id): IDX_I (0) before NSE_EQ (1).
        let order: Vec<SecurityId> = read
            .instruments()
            .iter()
            .map(|(i, _)| i.security_id)
            .collect();
        assert_eq!(order, vec![13, 2885]);
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap_or_else(|e| panic!("read_dir: {e}"))
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn test_read_absent_is_none_and_damaged_is_an_error() {
        let dir = unique_dir();
        assert!(matches!(read_record_in(dir.path(), day()), Ok(None)));
        let path = record_path_in(dir.path(), day());
        std::fs::write(&path, b"{not json").unwrap_or_else(|e| panic!("write: {e}"));
        assert!(read_record_in(dir.path(), day()).is_err());
        // Another day's content under this day's name is refused.
        std::fs::write(
            &path,
            br#"{"date_ist":"2026-10-29","master_listed":true,"index_fallback":false,"instruments":[]}"#,
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        assert!(read_record_in(dir.path(), day()).is_err());
        // A key held twice is refused.
        std::fs::write(
            &path,
            br#"{"date_ist":"2026-10-30","master_listed":true,"index_fallback":false,"instruments":[{"security_id":1,"segment":1,"first_secs_of_day":5},{"security_id":1,"segment":1,"first_secs_of_day":6}]}"#,
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        assert!(read_record_in(dir.path(), day()).is_err());
        // An over-cap file is refused before it is parsed.
        let big = vec![b' '; (DAY_RECORD_READ_CAP_BYTES + 1) as usize];
        std::fs::write(&path, big).unwrap_or_else(|e| panic!("write: {e}"));
        assert!(read_record_in(dir.path(), day()).is_err());
    }

    #[test]
    fn test_an_unreadable_record_is_replaced_and_reported() {
        let dir = unique_dir();
        std::fs::write(record_path_in(dir.path(), day()), b"garbage")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let a = inst(2885, ExchangeSegment::NseEquity);
        let out = record_in(dir.path(), day(), &[a], 30_000, UniverseBasis::MasterList)
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(out.replaced_unreadable);
        let read = read_record_in(dir.path(), day())
            .unwrap_or_else(|e| panic!("read: {e}"))
            .unwrap_or_else(|| panic!("record missing"));
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn test_write_sweeps_records_past_the_keep_and_leaves_foreign_files() {
        let dir = unique_dir();
        let old = day() - chrono::Duration::days(DAY_RECORD_KEEP_DAYS + 1);
        let kept = day() - chrono::Duration::days(DAY_RECORD_KEEP_DAYS);
        for d in [old, kept] {
            std::fs::write(record_path_in(dir.path(), d), b"{}")
                .unwrap_or_else(|e| panic!("write: {e}"));
        }
        let foreign = dir.path().join("depth-held-today.json");
        std::fs::write(&foreign, b"{}").unwrap_or_else(|e| panic!("write: {e}"));
        record_in(
            dir.path(),
            day(),
            &[inst(1, ExchangeSegment::NseEquity)],
            1,
            UniverseBasis::MasterList,
        )
        .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(!record_path_in(dir.path(), old).exists());
        assert!(record_path_in(dir.path(), kept).exists());
        assert!(foreign.exists());
    }

    #[test]
    fn test_a_failed_write_returns_err_and_leaves_no_temp_file() {
        let dir = unique_dir();
        // A directory squatting on the final name makes the rename fail.
        std::fs::create_dir_all(record_path_in(dir.path(), day()))
            .unwrap_or_else(|e| panic!("mkdir: {e}"));
        let mut r = SpotDayRecord::new(day());
        r.merge(
            &[inst(1, ExchangeSegment::NseEquity)],
            1,
            UniverseBasis::MasterList,
        );
        assert!(write_record_in(dir.path(), &r).is_err());
        let tmp = dir.path().join(format!(
            "{DAY_RECORD_PREFIX}{}{DAY_RECORD_TMP_SUFFIX}",
            day()
        ));
        assert!(!tmp.exists());
    }

    #[test]
    fn test_day_record_path_names_the_day() {
        assert!(
            day_record_path(day())
                .to_string_lossy()
                .ends_with("data/instrument-cache/main-feed-spot-2026-10-30.json")
        );
    }

    #[test]
    fn test_ist_secs_of_day_now_and_ist_date_now_read_the_ist_clock() {
        assert!(ist_secs_of_day_now() < 86_400);
        assert!(ist_date_now() > day() - chrono::Duration::days(3650));
    }

    #[test]
    fn test_record_spot_subscribed_then_read_spot_day_record_in_the_fixed_dir() {
        // A date far in the past so the fixed folder holds nothing of the
        // live day; the sweep only removes files older than this date.
        let d = NaiveDate::from_ymd_opt(1999, 1, 4).unwrap_or_default();
        let a = inst(424_242, ExchangeSegment::NseEquity);
        let out = record_spot_subscribed(d, &[a], 33_300, UniverseBasis::MasterList)
            .unwrap_or_else(|e| panic!("record: {e}"));
        assert!(out.recorded >= 1);
        let read = read_spot_day_record(d)
            .unwrap_or_else(|e| panic!("read: {e}"))
            .unwrap_or_else(|| panic!("record missing"));
        assert_eq!(read.first_secs_of_day(&a), Some(33_300));
        let _removed = std::fs::remove_file(day_record_path(d));
    }

    /// Review round 1: a noted set replaces only the earlier set of the same
    /// origin, so the boot set and the widened list are both recorded.
    #[test]
    fn test_note_process_held_keeps_one_set_per_origin_and_process_held_returns_them() {
        let a = SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::IdxI,
        };
        let b = SubscribeInstrument {
            security_id: 2_885,
            segment: ExchangeSegment::NseEquity,
        };
        note_process_held(HeldOrigin::Boot, &[a], UniverseBasis::IndexFallback);
        note_process_held(HeldOrigin::Widen, &[b], UniverseBasis::MasterList);
        note_process_held(HeldOrigin::Widen, &[a, b], UniverseBasis::MasterList);
        let held = process_held();
        assert_eq!(held.len(), 2);
        assert!(held.contains(&(HeldOrigin::Boot, vec![a], UniverseBasis::IndexFallback)));
        assert!(held.contains(&(HeldOrigin::Widen, vec![a, b], UniverseBasis::MasterList)));
    }

    /// Review round 2: a failed write is noted per IST day, read back for
    /// that day only, and cleared only for that day.
    #[test]
    fn test_note_record_write_failed_record_write_failed_for_and_clear_record_write_failed() {
        // Far-future days, so no other test's day collides.
        let day = NaiveDate::from_ymd_opt(2998, 1, 5).expect("date");
        let other = NaiveDate::from_ymd_opt(2998, 1, 6).expect("date");
        note_record_write_failed(day);
        assert!(record_write_failed_for(day));
        assert!(!record_write_failed_for(other));
        clear_record_write_failed(other);
        assert!(record_write_failed_for(day), "another day's clear keeps it");
        clear_record_write_failed(day);
        assert!(!record_write_failed_for(day));
    }
}
