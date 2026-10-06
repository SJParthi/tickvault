//! Once-per-trading-day delivery markers for daily scheduled tasks
//! (Telegram cleanliness overhaul, coordinator-relayed directive
//! 2026-07-15).
//!
//! The 15:40 IST timeframe check's `RunCatchUp` arm re-fired its summary on
//! EVERY post-15:40 restart — three "nothing to check" cards in one evening.
//! A daily marker file (`data/state/daily/<task>-YYYY-MM-DD.marker`) records
//! that a task's TERMINAL-quality summary was already delivered for the day,
//! so a later same-day restart's catch-up arm can skip the re-run. Since
//! 2026-09-24 the cross-verification marker also releases that day's S3
//! archive, so "the marker exists" must mean "the day was recorded".
//!
//! Contract (Rule-11 safe):
//! - **Atomic write** (2026-10-06, plan item 51a, `no-rest` §12.15.7): the
//!   contents go to `<task>-<date>.marker.tmp`, are fsynced, and are renamed
//!   onto the final name; then the folder is fsynced. A write returns
//!   [`std::io::Result`]`<`[`MarkerDurability`]`>`, so no caller can mistake a
//!   failed write for a recorded day. A crash leaves either no marker or a
//!   complete one.
//! - **Strict reader**: a marker counts only if it holds the exact line
//!   `delivered_date_ist=<date>`, read through a 4 KiB cap. An empty, torn,
//!   wrong-date or unreadable file reads as absent.
//! - **Fail-OPEN**: any IO uncertainty reads as "no marker" — the task runs
//!   and notifies again (noisy-not-silent; the scoreboard's never-skip-on-
//!   uncertainty precedent).
//! - Markers are ADVISORY: `rm data/state/daily/*` re-arms a day; no schema,
//!   no config key.
//! - Callers write a marker ONLY after a PASS outcome (G4a, fix round 2:
//!   `no_data` no longer seals a day — a trading-day no_data can be a
//!   silently-empty discovery dataset, the PR #1474 blind-since-birth
//!   class, so a same-day restart must re-run). FAILURE-class outcomes
//!   must never write one, so a restart re-runs AND re-pages.
//! - **Per-task keep horizon**: every write sweeps same-task markers (and
//!   same-task temp files) older than the caller's keep —
//!   [`DAILY_MARKER_SWEEP_DAYS`] by default, longer for a task whose marker
//!   is read back weeks later (the cross-verification S3 gate keeps 400).
//!
//! **Honest limit:** only the marker folder and, when this write created it,
//! its parent are fsynced. A newly created grandparent (`data/state`) is not,
//! so a host crash right after the first write can lose the marker. A lost
//! marker is the safe direction: the day re-runs or stays held.
//!
//! Cold path only (boot / once-a-day scheduling) — synchronous `std::fs` is
//! legal here, run through `off_worker`; nothing touches the tick hot path.
//! The sweep is O(files in the folder), at most about 414 files.

use chrono::NaiveDate;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Directory holding the per-day task markers (relative to the app working
/// directory, like the sibling `data/` state paths).
pub const DAILY_MARKER_DIR: &str = "data/state/daily";

/// Default keep horizon: same-task markers older than this many days are
/// swept on every write — bounded directory growth with zero external
/// scheduling.
pub const DAILY_MARKER_SWEEP_DAYS: i64 = 7;

/// Marker filename suffix.
const MARKER_SUFFIX: &str = ".marker";

/// Temp-file suffix used by the atomic write.
const MARKER_TMP_SUFFIX: &str = ".marker.tmp";

/// The most bytes the reader looks at. A real marker is about 60 bytes.
const MARKER_READ_CAP_BYTES: u64 = 4096;

/// How durable a successful marker write is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerDurability {
    /// The file and its folder were fsynced: the marker survives a host crash
    /// (within the honest limit in the module doc).
    Durable,
    /// The marker is in place under its final name and was fsynced, but the
    /// folder sync failed, so a host crash could lose the rename. The day
    /// counts as recorded; callers log a coded `warn!`.
    RenamedNotDirSynced,
}

// ---------------------------------------------------------------------------
// Base-dir-parameterized core (unit-testable without touching the repo CWD)
// ---------------------------------------------------------------------------

fn marker_path_in(base: &Path, task: &str, date_ist: NaiveDate) -> PathBuf {
    base.join(format!("{task}-{date_ist}{MARKER_SUFFIX}"))
}

fn marker_tmp_path_in(base: &Path, task: &str, date_ist: NaiveDate) -> PathBuf {
    base.join(format!("{task}-{date_ist}{MARKER_TMP_SUFFIX}"))
}

/// `true` iff the marker file holds the exact line
/// `delivered_date_ist=<date>` within its first 4 KiB. Any IO uncertainty,
/// an empty or torn file, or another day's line => `false` (fail-open: run +
/// notify again rather than silently skipping).
fn marker_exists_in(base: &Path, task: &str, date_ist: NaiveDate) -> bool {
    // Sweep S5: the read runs off the shared tokio worker.
    tickvault_storage::off_worker::off_worker(|| {
        marker_names_day(&marker_path_in(base, task, date_ist), date_ist)
    })
}

/// Reads at most [`MARKER_READ_CAP_BYTES`] of `path` and looks for the exact
/// `delivered_date_ist=<date>` line. A directory, a missing file or a read
/// error is `false`.
fn marker_names_day(path: &Path, date_ist: NaiveDate) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(128);
    if file
        .take(MARKER_READ_CAP_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    let want = format!("delivered_date_ist={date_ist}");
    bytes
        .split(|b| *b == b'\n')
        .any(|line| line == want.as_bytes())
}

/// What a successful rename becomes, given the folder syncs. `parent_sync` is
/// `None` when the folder already existed (its parent was not touched). A
/// failed folder sync never undoes the rename: the final file is in place and
/// the strict reader accepts it. Pure.
fn durability_after_rename(
    base_sync: std::io::Result<()>,
    parent_sync: Option<std::io::Result<()>>,
) -> MarkerDurability {
    let parent_ok = parent_sync.is_none_or(|r| r.is_ok());
    if base_sync.is_ok() && parent_ok {
        MarkerDurability::Durable
    } else {
        MarkerDurability::RenamedNotDirSynced
    }
}

/// Opens a folder and fsyncs it, so a rename inside it is durable.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    std::fs::File::open(dir)?.sync_all()
}

/// Atomic marker write: temp file, fsync, rename, folder fsync, then the
/// bounded same-task sweep with `keep_days`.
///
/// # Errors
/// Any failure up to and including the rename: the marker is then absent (or
/// the previous one is untouched), which is the safe direction. A folder-sync
/// failure AFTER the rename is not an error; it returns
/// [`MarkerDurability::RenamedNotDirSynced`].
fn write_marker_in(
    base: &Path,
    task: &str,
    date_ist: NaiveDate,
    keep_days: i64,
) -> std::io::Result<MarkerDurability> {
    let path = marker_path_in(base, task, date_ist);
    let tmp = marker_tmp_path_in(base, task, date_ist);
    let contents = format!(
        "build={}\ndelivered_date_ist={date_ist}\n",
        tickvault_common::build_info::BUILD_GIT_SHA
    );
    // Sweep S5: the write runs off the shared tokio worker.
    let durability = tickvault_storage::off_worker::off_worker(|| {
        // (a) Note whether the folder exists before creating it: a folder this
        // write creates must have its parent synced too.
        let base_existed = base.exists();
        std::fs::create_dir_all(base)?;
        // (b) Fixed temp name, truncated: a temp file a crash left behind is
        // overwritten, never appended to.
        let written = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .and_then(|mut file| {
                file.write_all(contents.as_bytes())?;
                file.sync_all()
            })
            // (c) Only a complete, synced file is renamed onto the final name.
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(err) = written {
            // Best-effort: a leftover temp file is overwritten by the next
            // write and swept once it ages out.
            let _removed = std::fs::remove_file(&tmp);
            return Err(err);
        }
        // (d) Folder sync. A failure here keeps the write a success.
        let base_sync = sync_dir(base);
        let parent_sync = (!base_existed).then(|| sync_dir(base.parent().unwrap_or(base)));
        Ok(durability_after_rename(base_sync, parent_sync))
    })?;
    // (e) Sweep only after a successful write; its errors are ignored.
    tickvault_storage::off_worker::off_worker(|| {
        sweep_old_markers_in(base, task, date_ist, keep_days);
    });
    Ok(durability)
}

/// Removes same-task markers and same-task temp files whose embedded date is
/// more than `keep_days` before `date_ist`. Unparsable or foreign filenames
/// are always left alone (fail-open — never delete what we did not write). A
/// negative `keep_days` is treated as 0, so today's marker is never swept; a
/// horizon too large to subtract sweeps nothing. O(files in `base`).
fn sweep_old_markers_in(base: &Path, task: &str, date_ist: NaiveDate, keep_days: i64) {
    let Some(cutoff) = chrono::TimeDelta::try_days(keep_days.max(0))
        .and_then(|keep| date_ist.checked_sub_signed(keep))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let prefix = format!("{task}-");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(stem) = rest
            .strip_suffix(MARKER_TMP_SUFFIX)
            .or_else(|| rest.strip_suffix(MARKER_SUFFIX))
        else {
            continue;
        };
        let Ok(marker_date) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
            continue;
        };
        if marker_date < cutoff {
            // Best-effort: a failed removal is retried on the next write.
            let _removed = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Public API (fixed base dir)
// ---------------------------------------------------------------------------

/// `data/state/daily/<task>-YYYY-MM-DD.marker` for the given task + IST day.
/// `task` must be a fixed lowercase slug (e.g. `"tf-consistency"`), never
/// user- or wire-derived text.
#[must_use]
pub fn daily_marker_path(task: &str, date_ist: NaiveDate) -> PathBuf {
    marker_path_in(Path::new(DAILY_MARKER_DIR), task, date_ist)
}

/// `true` iff the day's marker exists for this task AND names the day.
/// Fail-OPEN: any IO uncertainty, or an empty or torn file, reads `false`, so
/// the caller runs + notifies again.
#[must_use]
pub fn daily_marker_exists(task: &str, date_ist: NaiveDate) -> bool {
    marker_exists_in(Path::new(DAILY_MARKER_DIR), task, date_ist)
}

/// "Delivered today" marker write (contents: build sha + the delivered IST
/// date), atomic, with the default [`DAILY_MARKER_SWEEP_DAYS`] keep. Callers
/// invoke this ONLY after a PASS outcome — never on a FAILURE-class outcome,
/// and (G4a, fix round 2) never on `no_data` either: a trading-day no_data
/// can be a silently-empty discovery dataset, so marker-sealing it would hide
/// the day from every same-day restart's catch-up.
///
/// # Errors
/// The write did not land; see [`try_write_daily_marker_keeping`]. The caller
/// must log it with its own error code.
pub fn try_write_daily_marker(
    task: &str,
    date_ist: NaiveDate,
) -> std::io::Result<MarkerDurability> {
    try_write_daily_marker_keeping(task, date_ist, DAILY_MARKER_SWEEP_DAYS)
}

/// Like [`try_write_daily_marker`], keeping same-task markers for `keep_days`
/// instead of the default. For a task whose marker is read back long after it
/// was written (the cross-verification S3 gate).
///
/// # Errors
/// Any failure before the marker was renamed into place; the marker is then
/// absent (or the previous one is untouched).
pub fn try_write_daily_marker_keeping(
    task: &str,
    date_ist: NaiveDate,
    keep_days: i64,
) -> std::io::Result<MarkerDurability> {
    write_marker_in(Path::new(DAILY_MARKER_DIR), task, date_ist, keep_days)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard that removes its unique temp dir on drop (no `tempfile` dep —
    /// the house `TmpDir` pattern, originally from the retired
    /// `tick_conservation_boot.rs`).
    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    static TMP_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn unique_dir() -> TmpDir {
        let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tv_daily_marker_{}_{}", std::process::id(), n));
        TmpDir(dir)
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }

    fn write_ok(base: &Path, task: &str, d: NaiveDate, keep: i64) {
        let written = write_marker_in(base, task, d, keep);
        assert!(written.is_ok(), "marker write must succeed: {written:?}");
    }

    #[test]
    fn test_daily_marker_path_shape() {
        let p = daily_marker_path("tf-consistency", day("2026-07-15"));
        assert_eq!(
            p,
            PathBuf::from("data/state/daily/tf-consistency-2026-07-15.marker")
        );
        // The base-dir const is the path's parent — a drifted const would
        // orphan every existing marker.
        assert!(p.starts_with(DAILY_MARKER_DIR));
    }

    #[test]
    fn test_daily_marker_exists_false_when_absent_and_true_after_write() {
        let tmp = unique_dir();
        let d = day("2026-07-15");
        assert!(
            !marker_exists_in(&tmp.0, "tf-consistency", d),
            "absent marker must read false"
        );
        write_ok(&tmp.0, "tf-consistency", d, DAILY_MARKER_SWEEP_DAYS);
        assert!(
            marker_exists_in(&tmp.0, "tf-consistency", d),
            "written marker must read true"
        );
        // A DIFFERENT day's marker never satisfies today's check.
        assert!(!marker_exists_in(
            &tmp.0,
            "tf-consistency",
            day("2026-07-16")
        ));
        // A DIFFERENT task's marker never satisfies this task's check.
        assert!(!marker_exists_in(&tmp.0, "brutex-crossverify", d));
    }

    #[test]
    fn test_daily_marker_exists_fail_open_on_unreadable_base_dir() {
        // The base dir does not exist at all — IO uncertainty must read
        // `false` (run + notify again), never a panic and never `true`.
        let ghost = std::env::temp_dir().join(format!(
            "tv_daily_marker_ghost_{}_never_created",
            std::process::id()
        ));
        assert!(!marker_exists_in(
            &ghost,
            "tf-consistency",
            day("2026-07-15")
        ));
        // A DIRECTORY squatting on the marker path is not a marker file.
        let tmp = unique_dir();
        let d = day("2026-07-15");
        let squatter = marker_path_in(&tmp.0, "tf-consistency", d);
        std::fs::create_dir_all(&squatter).expect("create squatter dir");
        assert!(
            !marker_exists_in(&tmp.0, "tf-consistency", d),
            "a directory at the marker path must not read as delivered"
        );
    }

    #[test]
    fn test_write_marker_returns_err_on_unwritable_base() {
        // Base path squats on a FILE — create_dir_all fails; the write must
        // return Err (never panic) and the marker must read absent.
        let tmp = unique_dir();
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let file_as_base = tmp.0.join("not-a-dir");
        std::fs::write(&file_as_base, b"x").expect("write squatter file");
        let written = write_marker_in(
            &file_as_base,
            "tf-consistency",
            day("2026-07-15"),
            DAILY_MARKER_SWEEP_DAYS,
        );
        assert!(
            written.is_err(),
            "an unwritable base must be Err: {written:?}"
        );
        assert!(!marker_exists_in(
            &file_as_base,
            "tf-consistency",
            day("2026-07-15")
        ));
    }

    #[test]
    fn test_write_marker_returns_err_when_rename_target_is_a_directory() {
        let tmp = unique_dir();
        let d = day("2026-07-15");
        let squatter = marker_path_in(&tmp.0, "dhan_live_crossverify", d);
        std::fs::create_dir_all(&squatter).expect("create squatter dir");
        let written = write_marker_in(&tmp.0, "dhan_live_crossverify", d, 400);
        assert!(
            written.is_err(),
            "rename onto a directory must fail: {written:?}"
        );
        assert!(!marker_exists_in(&tmp.0, "dhan_live_crossverify", d));
        assert!(
            !marker_tmp_path_in(&tmp.0, "dhan_live_crossverify", d).exists(),
            "a failed write removes its temp file"
        );
    }

    #[test]
    fn test_write_marker_is_atomic_leaves_no_tmp_and_complete_contents() {
        let tmp = unique_dir();
        let d = day("2026-10-06");
        let written = write_marker_in(&tmp.0, "dhan_live_crossverify", d, 400);
        assert_eq!(written.ok(), Some(MarkerDurability::Durable));
        let contents = std::fs::read_to_string(marker_path_in(&tmp.0, "dhan_live_crossverify", d))
            .expect("read marker");
        assert!(contents.starts_with("build="), "{contents}");
        assert!(
            contents.ends_with("\ndelivered_date_ist=2026-10-06\n"),
            "{contents}"
        );
        assert!(!marker_tmp_path_in(&tmp.0, "dhan_live_crossverify", d).exists());
    }

    #[test]
    fn test_write_marker_overwrites_a_stale_tmp_left_by_a_crash() {
        let tmp = unique_dir();
        let d = day("2026-10-06");
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let stale = marker_tmp_path_in(&tmp.0, "dhan_live_crossverify", d);
        // Longer than the real contents, so an append or a missing truncate
        // would leave junk behind.
        std::fs::write(&stale, vec![b'z'; 8_192]).expect("write stale tmp");
        write_ok(&tmp.0, "dhan_live_crossverify", d, 400);
        assert!(!stale.exists(), "the temp file was renamed away");
        let contents = std::fs::read_to_string(marker_path_in(&tmp.0, "dhan_live_crossverify", d))
            .expect("read marker");
        assert!(!contents.contains('z'), "{contents}");
        assert!(marker_exists_in(&tmp.0, "dhan_live_crossverify", d));
    }

    #[test]
    fn test_marker_exists_rejects_empty_torn_or_wrong_date_marker() {
        let tmp = unique_dir();
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let d = day("2026-10-06");
        let path = marker_path_in(&tmp.0, "dhan_live_crossverify", d);
        for body in [
            "",
            "build=abc",
            "build=abc\n",
            "build=abc\ndelivered_date_ist=2026-10-0",
            "build=abc\ndelivered_date_ist=2026-10-05\n",
            "build=abc\n delivered_date_ist=2026-10-06\n",
            "build=abc\ndelivered_date_ist=2026-10-06 trailing\n",
        ] {
            std::fs::write(&path, body).expect("write marker");
            assert!(
                !marker_exists_in(&tmp.0, "dhan_live_crossverify", d),
                "{body:?} must read absent"
            );
        }
    }

    #[test]
    fn test_marker_exists_accepts_the_legacy_marker_format() {
        let tmp = unique_dir();
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let d = day("2026-10-06");
        let path = marker_path_in(&tmp.0, "dhan_live_crossverify", d);
        // The exact bytes the pre-2026-10-06 in-place write produced.
        std::fs::write(&path, "build=60bdfd97a\ndelivered_date_ist=2026-10-06\n")
            .expect("write legacy marker");
        assert!(marker_exists_in(&tmp.0, "dhan_live_crossverify", d));
        // No trailing newline still names the day.
        std::fs::write(&path, "build=60bdfd97a\ndelivered_date_ist=2026-10-06")
            .expect("write legacy marker");
        assert!(marker_exists_in(&tmp.0, "dhan_live_crossverify", d));
    }

    #[test]
    fn test_marker_exists_reads_at_most_4kib() {
        let tmp = unique_dir();
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let d = day("2026-10-06");
        let path = marker_path_in(&tmp.0, "dhan_live_crossverify", d);
        let mut body = String::from("build=abc\n");
        body.push_str(&"x".repeat(MARKER_READ_CAP_BYTES as usize));
        body.push_str("\ndelivered_date_ist=2026-10-06\n");
        std::fs::write(&path, &body).expect("write big marker");
        assert!(
            !marker_exists_in(&tmp.0, "dhan_live_crossverify", d),
            "a date line past the 4 KiB cap must not be read"
        );
        // The same line inside the cap is read.
        std::fs::write(&path, "delivered_date_ist=2026-10-06\n").expect("write marker");
        assert!(marker_exists_in(&tmp.0, "dhan_live_crossverify", d));
    }

    #[test]
    fn test_sweep_removes_stale_same_task_tmp_only() {
        let tmp = unique_dir();
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let today = day("2026-07-15");
        let stale_tmp = marker_tmp_path_in(&tmp.0, "tf-consistency", day("2026-07-01"));
        let fresh_tmp = marker_tmp_path_in(&tmp.0, "tf-consistency", day("2026-07-14"));
        let foreign_tmp = marker_tmp_path_in(&tmp.0, "other-task", day("2026-07-01"));
        let junk_tmp = tmp.0.join("tf-consistency-not-a-date.marker.tmp");
        for p in [&stale_tmp, &fresh_tmp, &foreign_tmp, &junk_tmp] {
            std::fs::write(p, b"partial").expect("write tmp");
        }
        write_ok(&tmp.0, "tf-consistency", today, DAILY_MARKER_SWEEP_DAYS);
        assert!(!stale_tmp.exists(), "a stale same-task temp file is swept");
        assert!(fresh_tmp.exists(), "a fresh temp file is kept");
        assert!(foreign_tmp.exists(), "another task's temp file is kept");
        assert!(junk_tmp.exists(), "an unparsable temp name is kept");
    }

    #[test]
    fn test_keep_days_parameter_controls_the_sweep_horizon() {
        let task = "dhan_live_crossverify";
        let today = day("2026-10-06");
        let old = day("2026-09-06");
        // keep 400: a 30-day-old marker survives.
        let long = unique_dir();
        write_ok(&long.0, task, old, 400);
        write_ok(&long.0, task, today, 400);
        assert!(marker_exists_in(&long.0, task, old));
        // keep 7: the same marker is swept.
        let short = unique_dir();
        write_ok(&short.0, task, old, 7);
        write_ok(&short.0, task, today, 7);
        assert!(!marker_exists_in(&short.0, task, old));
        assert!(marker_exists_in(&short.0, task, today));
        // A negative keep never sweeps today's marker; an absurd keep sweeps
        // nothing and never panics.
        let odd = unique_dir();
        write_ok(&odd.0, task, today, -5);
        assert!(marker_exists_in(&odd.0, task, today));
        write_ok(&odd.0, task, today, i64::MAX);
        assert!(marker_exists_in(&odd.0, task, today));
    }

    /// The write must fsync the file, then rename, then sync the folder — in
    /// that order — and a folder-sync failure must map to
    /// `RenamedNotDirSynced`, never to `Err`.
    #[test]
    fn test_write_marker_orders_sync_rename_dirsync() {
        let src = include_str!("daily_task_marker.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        let body = prod
            .split("fn write_marker_in(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        let file_sync = body.find("file.sync_all()").unwrap_or(usize::MAX);
        let rename = body.find("std::fs::rename(").unwrap_or(usize::MAX);
        let dir_sync = body.find("sync_dir(base)").unwrap_or(usize::MAX);
        assert!(file_sync < rename, "fsync the file before the rename");
        assert!(rename < dir_sync, "sync the folder after the rename");
        assert_ne!(dir_sync, usize::MAX, "the folder must be synced");
        assert!(
            !body.contains("sync_dir(base)?"),
            "a folder-sync failure must not turn into Err"
        );
        let failed = || Err(std::io::Error::other("sync failed"));
        assert_eq!(
            durability_after_rename(Ok(()), None),
            MarkerDurability::Durable
        );
        assert_eq!(
            durability_after_rename(Ok(()), Some(Ok(()))),
            MarkerDurability::Durable
        );
        assert_eq!(
            durability_after_rename(failed(), None),
            MarkerDurability::RenamedNotDirSynced
        );
        assert_eq!(
            durability_after_rename(Ok(()), Some(failed())),
            MarkerDurability::RenamedNotDirSynced
        );
    }

    #[test]
    fn test_write_daily_marker_sweeps_only_stale_same_task_markers() {
        let tmp = unique_dir();
        let today = day("2026-07-15");
        // Stale (> 7 days old), fresh (within window), foreign-task, and
        // unparsable neighbors.
        write_ok(&tmp.0, "tf-consistency", day("2026-07-01"), 7);
        write_ok(&tmp.0, "tf-consistency", day("2026-07-10"), 7);
        write_ok(&tmp.0, "brutex-crossverify", day("2026-07-01"), 7);
        let junk = tmp.0.join("tf-consistency-not-a-date.marker");
        std::fs::write(&junk, b"junk").expect("write junk");
        let unrelated = tmp.0.join("README.txt");
        std::fs::write(&unrelated, b"keep").expect("write unrelated");

        write_ok(&tmp.0, "tf-consistency", today, 7);

        // Stale same-task marker swept (2026-07-01 < 2026-07-08 cutoff).
        assert!(!marker_exists_in(
            &tmp.0,
            "tf-consistency",
            day("2026-07-01")
        ));
        // Fresh same-task marker kept (2026-07-10 >= cutoff).
        assert!(marker_exists_in(
            &tmp.0,
            "tf-consistency",
            day("2026-07-10")
        ));
        // Today's marker written.
        assert!(marker_exists_in(&tmp.0, "tf-consistency", today));
        // Foreign task NEVER swept by this task's write.
        assert!(marker_exists_in(
            &tmp.0,
            "brutex-crossverify",
            day("2026-07-01")
        ));
        // Unparsable / unrelated files are left alone (fail-open).
        assert!(junk.is_file(), "unparsable marker-lookalike must survive");
        assert!(unrelated.is_file(), "unrelated file must survive");
    }

    #[test]
    fn test_write_daily_marker_contents_carry_build_and_date() {
        let tmp = unique_dir();
        let d = day("2026-07-15");
        write_ok(&tmp.0, "tf-consistency", d, DAILY_MARKER_SWEEP_DAYS);
        let contents = std::fs::read_to_string(marker_path_in(&tmp.0, "tf-consistency", d))
            .expect("read marker");
        assert!(contents.contains("build="), "{contents}");
        assert!(
            contents.contains("delivered_date_ist=2026-07-15"),
            "{contents}"
        );
    }

    /// The public writers use the fixed folder; both exist with a real
    /// production caller (pub-fn wiring).
    #[test]
    fn test_try_write_daily_marker_keeping_backs_the_default_writer() {
        let src = include_str!("daily_task_marker.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        assert!(
            prod.contains(
                "try_write_daily_marker_keeping(task, date_ist, DAILY_MARKER_SWEEP_DAYS)"
            )
        );
        assert!(
            !prod.contains("pub fn write_daily_marker("),
            "the unit-returning writer must stay removed"
        );
    }
}
