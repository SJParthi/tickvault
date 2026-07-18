//! Once-per-trading-day delivery markers for daily scheduled tasks
//! (Telegram cleanliness overhaul, coordinator-relayed directive
//! 2026-07-15).
//!
//! The 15:40 IST timeframe check's `RunCatchUp` arm re-fired its summary on
//! EVERY post-15:40 restart — three "nothing to check" cards in one evening.
//! A daily marker file (`data/state/daily/<task>-YYYY-MM-DD.marker`) records
//! that a task's TERMINAL-quality summary was already delivered for the day,
//! so a later same-day restart's catch-up arm can skip the re-run.
//!
//! Contract (Rule-11 safe):
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
//! - Bounded growth: every write sweeps same-task markers older than
//!   [`DAILY_MARKER_SWEEP_DAYS`].
//!
//! Cold path only (boot / once-a-day scheduling) — synchronous `std::fs` is
//! legal here; nothing touches the tick hot path.

use chrono::NaiveDate;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Directory holding the per-day task markers (relative to the app working
/// directory, like the sibling `data/` state paths).
pub const DAILY_MARKER_DIR: &str = "data/state/daily";

/// Same-task markers older than this many days are swept on every write —
/// bounded directory growth with zero external scheduling.
pub const DAILY_MARKER_SWEEP_DAYS: i64 = 7;

/// Marker filename suffix.
const MARKER_SUFFIX: &str = ".marker";

// ---------------------------------------------------------------------------
// Base-dir-parameterized core (unit-testable without touching the repo CWD)
// ---------------------------------------------------------------------------

fn marker_path_in(base: &Path, task: &str, date_ist: NaiveDate) -> PathBuf {
    base.join(format!("{task}-{date_ist}{MARKER_SUFFIX}"))
}

/// `true` iff the marker file EXISTS. Any IO uncertainty => `false`
/// (fail-open: run + notify again rather than silently skipping).
fn marker_exists_in(base: &Path, task: &str, date_ist: NaiveDate) -> bool {
    marker_path_in(base, task, date_ist).is_file()
}

/// Best-effort marker write + bounded old-marker sweep. Never returns an
/// error — a failure logs ONE `warn!` and the day simply re-notifies after a
/// restart (fail-open, Rule-11 direction).
fn write_marker_in(base: &Path, task: &str, date_ist: NaiveDate) {
    let path = marker_path_in(base, task, date_ist);
    let contents = format!(
        "build={}\ndelivered_date_ist={date_ist}\n",
        tickvault_common::build_info::BUILD_GIT_SHA
    );
    let write_result = std::fs::create_dir_all(base).and_then(|()| std::fs::write(&path, contents));
    if let Err(err) = write_result {
        // Deliberately NOT an error! and NOT a flush/persist phrase — the
        // marker is advisory; losing it only means one duplicate card after
        // a restart (noisy-not-silent).
        warn!(
            ?err,
            task, "daily-task marker could not be written — the task may re-notify after a restart"
        );
        return;
    }
    sweep_old_markers_in(base, task, date_ist);
}

/// Removes same-task markers whose embedded date is older than
/// [`DAILY_MARKER_SWEEP_DAYS`] before `date_ist`. Unparsable or foreign
/// filenames are always left alone (fail-open — never delete what we did
/// not write).
fn sweep_old_markers_in(base: &Path, task: &str, date_ist: NaiveDate) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let cutoff = date_ist - chrono::Duration::days(DAILY_MARKER_SWEEP_DAYS);
    let prefix = format!("{task}-");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(MARKER_SUFFIX))
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

/// `true` iff the day's marker EXISTS for this task. Fail-OPEN: any IO
/// uncertainty reads `false`, so the caller runs + notifies again.
#[must_use]
pub fn daily_marker_exists(task: &str, date_ist: NaiveDate) -> bool {
    marker_exists_in(Path::new(DAILY_MARKER_DIR), task, date_ist)
}

/// Best-effort "delivered today" marker write (advisory contents: build sha
/// + the delivered IST date) plus the bounded old-marker sweep. Callers
/// invoke this ONLY after a PASS summary was dispatched — never on a
/// FAILURE-class outcome, and (G4a, fix round 2) never on `no_data`
/// either: a trading-day no_data can be a silently-empty discovery
/// dataset, so marker-sealing it would hide the day from every same-day
/// restart's catch-up.
pub fn write_daily_marker(task: &str, date_ist: NaiveDate) {
    write_marker_in(Path::new(DAILY_MARKER_DIR), task, date_ist);
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
        write_marker_in(&tmp.0, "tf-consistency", d);
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
    fn test_write_daily_marker_is_best_effort_never_panics_on_bad_base() {
        // Base path squats on a FILE — create_dir_all fails; the write must
        // degrade to the one warn! and return (fail-open, no panic).
        let tmp = unique_dir();
        std::fs::create_dir_all(&tmp.0).expect("create tmp");
        let file_as_base = tmp.0.join("not-a-dir");
        std::fs::write(&file_as_base, b"x").expect("write squatter file");
        write_marker_in(&file_as_base, "tf-consistency", day("2026-07-15"));
        assert!(!marker_exists_in(
            &file_as_base,
            "tf-consistency",
            day("2026-07-15")
        ));
    }

    #[test]
    fn test_write_daily_marker_sweeps_only_stale_same_task_markers() {
        let tmp = unique_dir();
        let today = day("2026-07-15");
        // Stale (> 7 days old), fresh (within window), foreign-task, and
        // unparsable neighbors.
        write_marker_in(&tmp.0, "tf-consistency", day("2026-07-01"));
        write_marker_in(&tmp.0, "tf-consistency", day("2026-07-10"));
        write_marker_in(&tmp.0, "brutex-crossverify", day("2026-07-01"));
        let junk = tmp.0.join("tf-consistency-not-a-date.marker");
        std::fs::write(&junk, b"junk").expect("write junk");
        let unrelated = tmp.0.join("README.txt");
        std::fs::write(&unrelated, b"keep").expect("write unrelated");

        write_marker_in(&tmp.0, "tf-consistency", today);

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
        write_marker_in(&tmp.0, "tf-consistency", d);
        let contents = std::fs::read_to_string(marker_path_in(&tmp.0, "tf-consistency", d))
            .expect("read marker");
        assert!(contents.contains("build="), "{contents}");
        assert!(
            contents.contains("delivered_date_ist=2026-07-15"),
            "{contents}"
        );
    }
}
