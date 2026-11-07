//! The daily archive leg's free-space target (plan item PR AA, step 5;
//! owner Quote 51, 2026-11-07).
//!
//! # What it is
//!
//! After the close the daily leg keeps running until free space on the data
//! volume reaches a target:
//!
//! `max(multiplier × p95 session bytes, pct × disk size)`
//!
//! with the shipped `[partition_retention]` defaults 150% and 30%. "Session
//! bytes" is what the five Quote 51 tables (`ticks`, `tick_quotes`,
//! `market_depth`, `market_depth_book`, `feed_aux_packets`) hold on disk for
//! one session day, read once after 15:40 IST
//! (`PartitionArchiver::session_bytes_on`). The last
//! [`SESSION_BYTES_RING_DAYS`] days are kept in
//! `data/state/archive_session_bytes.json`. With fewer than
//! [`SESSION_BYTES_MIN_DAYS`] days measured, the target uses
//! `daily_free_target_fallback_session_bytes` (40 GiB) instead of the p95.
//!
//! # The file
//!
//! JSON, written to a temp file, fsynced, renamed over the old one, then the
//! folder is fsynced, so a crash leaves either the old file or the new one,
//! never half of one. A file that is not exactly the expected shape (wrong
//! version, more than 20 days, days out of order or repeated, an unreadable
//! date) is REFUSED as a whole: the leg then uses the fallback until it has
//! measured five days again. The refused file holds nothing the target can
//! trust, so the next save replaces it.
//!
//! # Honest limits
//!
//! - A session day is read once, before this process drops any partition of
//!   that day. Hours the disk-pressure leg already dropped during the session
//!   are not counted, so a pressure day reads low.
//! - A day that reads 0 bytes (a holiday, or a box that wrote nothing) is not
//!   recorded, so it never pulls the p95 down.
//! - The target is not capped at the disk size; a target the disk cannot
//!   reach only keeps the leg polling until 07:30 IST.
//!
//! # Complexity
//!
//! Everything here is cold: at most 20 entries, read once at boot and
//! written at most once a day. Sorting 20 numbers is O(n log n) on n ≤ 20.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tickvault_common::config::PartitionRetentionConfig;

/// The file name, under `<data_dir>/state/`.
pub const SESSION_BYTES_FILE: &str = "archive_session_bytes.json";

/// How many session days the file keeps (the newest ones).
pub const SESSION_BYTES_RING_DAYS: usize = 20;

/// The p95 is used only once this many days are measured.
pub const SESSION_BYTES_MIN_DAYS: usize = 5;

/// The file format version; any other value is refused.
const RING_VERSION: u32 = 1;

/// Largest file the loader reads; 20 entries need under 2 KiB.
const RING_READ_CAP_BYTES: u64 = 64 * 1024;

/// The date format of each entry.
const DAY_FORMAT: &str = "%Y-%m-%d";

/// Percent divisor for the two target terms.
const PERCENT: u128 = 100;

/// One file entry as written on disk.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RingEntry {
    day: String,
    bytes: u64,
}

/// The file as written on disk.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RingFile {
    version: u32,
    days: Vec<RingEntry>,
}

/// Bytes the five Quote 51 tables held for each measured session day,
/// oldest first, one entry per day, at most [`SESSION_BYTES_RING_DAYS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionBytesRing {
    days: Vec<(NaiveDate, u64)>,
}

impl SessionBytesRing {
    /// Whether `day` is already measured. O(n), n ≤ 20.
    #[must_use]
    pub fn has_day(&self, day: NaiveDate) -> bool {
        self.days.iter().any(|(d, _)| *d == day)
    }

    /// Records `bytes` for `day`, replacing an earlier reading of the same
    /// day, and keeps only the newest [`SESSION_BYTES_RING_DAYS`] days.
    pub fn record(&mut self, day: NaiveDate, bytes: u64) {
        self.days.retain(|(d, _)| *d != day);
        self.days.push((day, bytes));
        self.days.sort_unstable_by_key(|(d, _)| *d);
        let excess = self.days.len().saturating_sub(SESSION_BYTES_RING_DAYS);
        self.days.drain(..excess);
    }

    /// Measured days held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.days.len()
    }

    /// Whether no day is measured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.days.is_empty()
    }

    /// The 95th percentile of the measured days (nearest rank), or `None`
    /// with fewer than [`SESSION_BYTES_MIN_DAYS`] days.
    #[must_use]
    pub fn p95_bytes(&self) -> Option<u64> {
        if self.days.len() < SESSION_BYTES_MIN_DAYS {
            return None;
        }
        let mut sorted: Vec<u64> = self.days.iter().map(|(_, b)| *b).collect();
        sorted.sort_unstable();
        // Nearest rank: ceil(0.95 × n), 1-based.
        let rank = (sorted.len() * 95).div_ceil(100);
        sorted.get(rank.saturating_sub(1)).copied()
    }

    /// The file text. Infallible: the shape is plain integers and strings.
    fn to_json(&self) -> String {
        let file = RingFile {
            version: RING_VERSION,
            days: self
                .days
                .iter()
                .map(|(d, b)| RingEntry {
                    day: d.format(DAY_FORMAT).to_string(),
                    bytes: *b,
                })
                .collect(),
        };
        serde_json::to_string(&file).unwrap_or_default()
    }

    /// Parses the file text, refusing anything not exactly the written shape.
    fn from_json(text: &str) -> Result<Self, String> {
        let file: RingFile =
            serde_json::from_str(text).map_err(|err| format!("not the ring format: {err}"))?;
        if file.version != RING_VERSION {
            return Err(format!("version {} is not {RING_VERSION}", file.version));
        }
        if file.days.len() > SESSION_BYTES_RING_DAYS {
            return Err(format!(
                "{} days is more than {SESSION_BYTES_RING_DAYS}",
                file.days.len()
            ));
        }
        let mut days = Vec::with_capacity(file.days.len());
        for entry in file.days {
            let day = NaiveDate::parse_from_str(&entry.day, DAY_FORMAT)
                .map_err(|_| format!("unreadable day {:?}", entry.day))?;
            if days.last().is_some_and(|(prev, _)| *prev >= day) {
                return Err(format!("day {day} is out of order or repeated"));
            }
            days.push((day, entry.bytes));
        }
        Ok(Self { days })
    }
}

/// `<data_dir>/state/archive_session_bytes.json`.
#[must_use]
pub fn session_bytes_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state").join(SESSION_BYTES_FILE)
}

/// Loads the ring. A missing file is an empty ring; any other read failure
/// or a file of the wrong shape is an error naming why (the caller then
/// uses an empty ring, so the target falls back).
///
/// # Errors
/// The file could not be read, is over the size cap, or is refused.
pub fn load_session_bytes(path: &Path) -> Result<SessionBytesRing, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionBytesRing::default());
        }
        Err(err) => return Err(format!("could not open: {err}")),
    };
    let mut text = String::new();
    file.take(RING_READ_CAP_BYTES.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|err| format!("could not read: {err}"))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > RING_READ_CAP_BYTES {
        return Err(format!("larger than {RING_READ_CAP_BYTES} bytes"));
    }
    SessionBytesRing::from_json(&text)
}

/// Saves the ring atomically: temp file, fsync, rename, folder fsync. Runs
/// off the shared tokio worker.
///
/// # Errors
/// Any failure up to and including the rename (the old file is then left
/// as it was). A failed folder fsync after the rename is also returned, so
/// the caller can say the save may not survive a power cut.
pub fn save_session_bytes(path: &Path, ring: &SessionBytesRing) -> std::io::Result<()> {
    let text = ring.to_json();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    tickvault_storage::off_worker::off_worker(|| {
        std::fs::create_dir_all(dir)?;
        let written = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(err) = written {
            // Best effort: a leftover temp file is overwritten next time.
            let _removed = std::fs::remove_file(&tmp);
            return Err(err);
        }
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        std::fs::File::open(dir)?.sync_all()
    })
}

/// The free-space target in bytes for a disk of `disk_total_bytes`:
/// `max(session × multiplier%, disk × pct%)`, with `session` the ring's p95
/// or, under five measured days, the configured fallback. Saturating, so no
/// configuration can wrap it. Pure, O(n log n) on n ≤ 20.
#[must_use]
pub fn free_target_bytes(
    ring: &SessionBytesRing,
    cfg: &PartitionRetentionConfig,
    disk_total_bytes: u64,
) -> u64 {
    let session = ring
        .p95_bytes()
        .unwrap_or(cfg.daily_free_target_fallback_session_bytes);
    let by_session = u128::from(session)
        .saturating_mul(u128::from(cfg.daily_free_target_session_multiplier_pct))
        / PERCENT;
    let by_disk = u128::from(disk_total_bytes)
        .saturating_mul(u128::from(cfg.daily_free_target_pct))
        / PERCENT;
    u64::try_from(by_session.max(by_disk)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 1)
            .and_then(|d| d.checked_add_days(chrono::Days::new(u64::from(n))))
            .expect("date")
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "tv-archive-free-target-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// The target is the larger of the two terms; under five measured days
    /// the fallback session size is used, from five days on the p95.
    #[test]
    fn test_free_target_p95_and_empty_ring_fallback() {
        let cfg = PartitionRetentionConfig::default();
        let disk = 150 * GIB;
        // Empty ring: 1.5 × 40 GiB = 60 GiB beats 30% of 150 GiB = 45 GiB.
        let empty = SessionBytesRing::default();
        assert_eq!(empty.p95_bytes(), None);
        assert_eq!(free_target_bytes(&empty, &cfg, disk), 60 * GIB);
        // Four days are still the fallback, however small they are.
        let mut ring = SessionBytesRing::default();
        for n in 0..4 {
            ring.record(day(n), GIB);
        }
        assert_eq!(ring.p95_bytes(), None);
        assert_eq!(free_target_bytes(&ring, &cfg, disk), 60 * GIB);
        // Five small days: p95 = 1 GiB, so 30% of the disk wins.
        ring.record(day(4), GIB);
        assert_eq!(ring.p95_bytes(), Some(GIB));
        assert_eq!(free_target_bytes(&ring, &cfg, disk), disk * 30 / 100);
        // Twenty days 1..=20 GiB: nearest-rank p95 is the 19th value.
        let mut big = SessionBytesRing::default();
        for n in 0..20 {
            big.record(day(n), u64::from(n + 1) * GIB);
        }
        assert_eq!(big.p95_bytes(), Some(19 * GIB));
        // 1.5 × 19 GiB = 28.5 GiB is under 30% of 150 GiB = 45 GiB.
        assert_eq!(free_target_bytes(&big, &cfg, disk), disk * 30 / 100);
        // On a 50 GiB disk the session term (28.5 GiB) wins over 15 GiB.
        assert_eq!(free_target_bytes(&big, &cfg, 50 * GIB), 19 * GIB * 3 / 2);
        // One huge outlier in twenty sits at rank 20, above the p95 rank (19).
        let mut spiky = big.clone();
        spiky.record(day(0), 1_000 * GIB);
        assert_eq!(spiky.p95_bytes(), Some(20 * GIB));
        // Saturating: no configuration wraps.
        let wild = PartitionRetentionConfig {
            daily_free_target_session_multiplier_pct: u32::MAX,
            daily_free_target_fallback_session_bytes: u64::MAX,
            ..PartitionRetentionConfig::default()
        };
        assert_eq!(free_target_bytes(&empty, &wild, disk), u64::MAX);
    }

    #[test]
    fn test_record_replaces_the_same_day_and_keeps_the_newest_twenty() {
        let mut ring = SessionBytesRing::default();
        ring.record(day(3), 5);
        ring.record(day(3), 7);
        assert_eq!(ring.len(), 1);
        assert!(ring.has_day(day(3)));
        for n in 0..30 {
            ring.record(day(29 - n), u64::from(n));
        }
        assert_eq!(ring.len(), SESSION_BYTES_RING_DAYS);
        assert!(!ring.has_day(day(9)), "the oldest days leave first");
        assert!(ring.has_day(day(10)) && ring.has_day(day(29)));
        assert!(!ring.is_empty());
    }

    /// The file round-trips through the atomic write, a missing file is an
    /// empty ring, and every malformed shape is refused whole.
    #[test]
    fn test_ring_file_atomic_write_and_corrupt_refused() {
        let dir = unique_dir("ring");
        let path = session_bytes_path(&dir);
        assert!(path.ends_with("state/archive_session_bytes.json"));
        assert_eq!(load_session_bytes(&path), Ok(SessionBytesRing::default()));

        let mut ring = SessionBytesRing::default();
        for n in 0..6 {
            ring.record(day(n), u64::from(n) * GIB);
        }
        save_session_bytes(&path, &ring).expect("saves");
        assert_eq!(load_session_bytes(&path), Ok(ring.clone()));
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists(), "no temp file is left behind");

        // A second save replaces the file whole.
        ring.record(day(7), 1);
        save_session_bytes(&path, &ring).expect("saves again");
        assert_eq!(load_session_bytes(&path), Ok(ring));

        let refused = [
            "",
            "{",
            "[]",
            r#"{"version":2,"days":[]}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"days":[{"day":"2026-13-01","bytes":1}]}"#,
            r#"{"version":1,"days":[{"day":"2026-11-02","bytes":1},{"day":"2026-11-01","bytes":1}]}"#,
            r#"{"version":1,"days":[{"day":"2026-11-01","bytes":1},{"day":"2026-11-01","bytes":2}]}"#,
            r#"{"version":1,"days":[{"day":"2026-11-01","bytes":-1}]}"#,
            r#"{"version":1,"days":[{"day":"2026-11-01","bytes":1,"x":0}]}"#,
            r#"{"version":1,"days":[],"extra":true}"#,
        ];
        for text in refused {
            std::fs::write(&path, text).expect("write fixture");
            assert!(load_session_bytes(&path).is_err(), "must refuse {text:?}");
        }
        let too_many: Vec<String> = (0..21)
            .map(|n| format!(r#"{{"day":"{}","bytes":1}}"#, day(n).format(DAY_FORMAT)))
            .collect();
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"days":[{}]}}"#, too_many.join(",")),
        )
        .expect("write fixture");
        assert!(load_session_bytes(&path).is_err(), "21 days is refused");
        std::fs::write(&path, " ".repeat(70 * 1024)).expect("write fixture");
        assert!(load_session_bytes(&path).is_err(), "over the size cap");

        let _cleanup = std::fs::remove_dir_all(&dir);
    }
}
