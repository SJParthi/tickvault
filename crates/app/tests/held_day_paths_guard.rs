//! Held-day paths guard (daily-universe Quote 63, owner cards 2026-11-20).
//!
//! A held day (2026-11-20, whose Dhan trade times arrived 5 h 30 min early,
//! or a day the feed-time page holds) must stay out of THREE paths until a
//! verified repair writes its `.done`:
//!
//! 1. the partition archiver (the daily leg, the hour path and the disk-pressure
//!    leg all run one pass function), for `ticks`, `tick_quotes`,
//!    `feed_aux_packets` and `candles_*`;
//! 2. the Brutex daily export, forced and last-chance runs included;
//! 3. the cross-check past-day catch-up.
//!
//! The source scans below fail the build when any of the three call sites
//! stops consulting the hold, when the hold moves after the per-pass cap or
//! the backlog measure, or when `main` could spawn the cross-check before the
//! held days are installed. The behaviour test drives, against real state
//! files: `HeldDays::holds_partition` (the same `archive_verdict` the pass
//! filter calls through `split_held_partitions`), `HeldDays::is_held` (the
//! exact check the export loop and `export_day` make; the loop's `continue`
//! is pinned by a scan, not run), and the cross-check's own
//! `past_days_to_check`. Bite-proven: each order scan has a negative
//! fixture.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use tickvault_storage::held_days::{HeldDays, MarkerKind, write_day_marker};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The text of `src` from `start` to the first `end` after it.
fn body<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
    let from = src
        .find(start)
        .unwrap_or_else(|| panic!("`{start}` not found"));
    let rest = &src[from..];
    let to = rest.find(end).unwrap_or(rest.len());
    &rest[..to]
}

/// Position of `needle` in `hay`, failing loudly.
fn at(hay: &str, needle: &str, what: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

/// The archive pass checks held days BEFORE the cross-verification hold,
/// the backlog measure and the per-pass limits. `Err` names the defect.
fn archive_pass_order(pass: &str) -> Result<(), String> {
    let held = pass
        .find("self.apply_held_days(worklist, &mut summary)")
        .ok_or("the pass does not apply the held days")?;
    for later in [
        "self.apply_crossverify_hold(worklist, &mut summary)",
        "self.worklist_sizes(&worklist, &hour_windows)",
        "apply_pass_limits(",
    ] {
        let pos = pass
            .find(later)
            .ok_or_else(|| format!("`{later}` missing from the pass"))?;
        if pos < held {
            return Err(format!("the held-day filter runs after `{later}`"));
        }
    }
    Ok(())
}

#[test]
fn held_day_paths_archive_pass_filters_before_cap_and_backlog() {
    let src = read("crates/storage/src/partition_archive.rs");
    let pass = body(
        &src,
        "async fn run_archive_pass(&mut self)",
        "async fn place_archive(",
    );
    archive_pass_order(pass).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        pass.contains("held_day_filter::count_held_day(0);"),
        "the held_day skip series must exist from the first pass"
    );
    let sizes = body(
        &src,
        "async fn worklist_sizes(",
        "pub async fn archive_and_drop",
    );
    assert!(
        sizes.contains("self.backlog_counts_held(table, p)"),
        "a held day's later hours must not count as drain-hold work"
    );
    // Both constructors read the config's held days.
    assert_eq!(
        src.matches("HeldDays::from_config(").count(),
        2,
        "PartitionArchiver::new and from_parts must both build the held days"
    );
    let filter = read("crates/storage/src/partition_archive/held_day_filter.rs");
    assert!(filter.contains("held_days.archive_verdict(day)"));
    // A `.hold` written mid-pass stops the copy at the next partition.
    let recheck = at(
        pass,
        "self.held_since_pass_start(table, &partition, &mut summary)",
        "run_archive_pass",
    );
    assert!(
        recheck
            < at(
                pass,
                "self.process_one(table, &partition, &mut summary)",
                "run_archive_pass"
            )
    );
    assert!(
        recheck
            < at(
                pass,
                "archive_with_snapshot_gate(self, table",
                "run_archive_pass"
            )
    );
    assert!(filter.contains("\"reason\" => \"held_day\""));
}

#[test]
fn held_day_paths_archive_order_detector_bites() {
    let wrong = "self.apply_crossverify_hold(worklist, &mut summary)\n\
                 self.apply_held_days(worklist, &mut summary)\n\
                 self.worklist_sizes(&worklist, &hour_windows)\napply_pass_limits(";
    assert!(archive_pass_order(wrong).is_err());
    assert!(archive_pass_order("apply_pass_limits(").is_err());
    let right = "self.apply_held_days(worklist, &mut summary)\n\
                 self.apply_crossverify_hold(worklist, &mut summary)\n\
                 self.worklist_sizes(&worklist, &hour_windows)\napply_pass_limits(";
    assert!(archive_pass_order(right).is_ok());
}

/// The export loop's held-day block ends in `continue` before the session
/// day is classified (a skip that only logs would run the day). `Err`
/// names the defect.
fn held_skip_continues(lp: &str) -> Result<(), String> {
    let start = lp
        .find("held_days.is_held(day))")
        .ok_or("the loop does not ask whether the day is held")?;
    let end = lp[start..]
        .find("let is_session_day")
        .ok_or("`let is_session_day` missing after the held check")?;
    if lp[start..start + end].contains("continue;") {
        Ok(())
    } else {
        Err("the held-day block does not `continue`".to_string())
    }
}

#[test]
fn held_day_paths_held_skip_detector_bites() {
    let wrong = "if held_days.is_held(day)) { info!(\"held\"); }\nlet is_session_day = 1;";
    assert!(held_skip_continues(wrong).is_err());
    assert!(held_skip_continues("let is_session_day").is_err());
    let right = "if held_days.is_held(day)) { continue;\n }\nlet is_session_day = 1;";
    assert!(held_skip_continues(right).is_ok());
}

#[test]
fn held_day_paths_export_skips_before_any_attempt_or_object() {
    // The export itself refuses a held day before its DONE check and before
    // any table is read.
    let export = read("crates/storage/src/partition_archive/daily_export.rs");
    let day_fn = body(&export, "pub async fn export_day(", "let archived = self");
    let held = at(day_fn, "self.archiver.held_days.is_held(day)", "export_day");
    assert!(held < at(day_fn, "self.is_sealed(day)", "export_day"));
    assert!(held < at(day_fn, "for table in export_tables()", "export_day"));
    assert!(day_fn.contains("return Ok(held_stop(day, \"start\"));"));
    let stop_fn = body(&export, "fn held_stop(", "\n}\n");
    assert!(stop_fn.contains("ExportOutcome::Stopped(StopReason::Held)"));
    assert!(stop_fn.contains("\"outcome\" => \"held_day\""));
    // A `.hold` written mid-export stops it before the next table.
    let tables = at(day_fn, "for table in export_tables()", "export_day");
    assert!(tables < at(day_fn, "return Ok(held_stop(day, table));", "export_day"));
    assert!(
        !day_fn.contains("stopped(day, \"held_day\""),
        "a held stop is counted once, as held_day"
    );

    // The scheduler skips a held day before it counts an attempt, checks a
    // DONE, or can mark the session day pending (the drain hold).
    let boot = read("crates/app/src/brutex_export_boot.rs");
    let lp = body(&boot, "async fn run_export_loop(", "#[cfg(test)]");
    assert!(lp.contains("HeldDays::from_config(&cfg)"));
    assert!(lp.contains("visit_order_with_released(session_day, &released)"));
    assert!(lp.contains("for day in visit {"));
    let held = at(lp, "held_days.is_held(day)", "run_export_loop");
    for later in [
        "days.entry(day).or_default()",
        "daily_marker_exists(EXPORT_MARKER_TASK, day)",
        "ex.is_sealed(day)",
        "past_day_forced(eligible, session_day)",
        "DayStep::Run =>",
        "session_pending |= is_session_day",
    ] {
        assert!(
            held < at(lp, later, "run_export_loop"),
            "the held-day skip must come before `{later}`"
        );
    }
    assert!(
        lp.contains("Ok(ExportOutcome::Stopped(StopReason::Held))"),
        "a hold written mid-run must not count as an attempt"
    );
    held_skip_continues(lp).unwrap_or_else(|e| panic!("{e}"));
    // Every way the loop learns a released day is sealed records it, so the
    // archiver (which waits for that) copies the repaired day after the
    // export placed it in order.
    assert_eq!(
        lp.matches("note_released_day_exported(&held_days, &released, day);")
            .count(),
        4,
        "the in-process seal, the local done cache, the DONE check and the run"
    );
}

#[test]
fn held_day_paths_cross_check_catch_up_consults_the_hold() {
    let src = read("crates/app/src/dhan_live_crossverify_boot.rs");
    let past = body(
        &src,
        "async fn run_past_days(",
        "pub fn spawn_dhan_live_crossverify(",
    );
    assert!(past.contains("tickvault_storage::held_days::process_held_days()"));
    assert!(past.contains("held_days.is_held(d)"));
    let decide = body(
        &src,
        "pub fn past_days_to_check(",
        "pub const fn past_day_worst_case_secs",
    );
    assert!(decide.contains("!is_held(*d)"));

    // The cross-check reads the process held days, installed first thing by
    // the daily archive spawn, which `main` runs before the cross-check.
    let archive = read("crates/app/src/daily_archive_boot.rs");
    let spawn = body(
        &archive,
        "pub fn spawn_supervised_daily_archive_loop(",
        "let Some(window_open)",
    );
    let install = at(spawn, "install_process_held_days(&cfg)", "archive spawn");
    assert!(install < at(spawn, "if !cfg.archive_enabled", "archive spawn"));
    let main = read("crates/app/src/main.rs");
    assert!(
        at(&main, "spawn_supervised_daily_archive_loop(", "main")
            < at(&main, "spawn_dhan_live_crossverify(", "main"),
        "main must install the held days before it spawns the cross-check"
    );
}

// PR-6 (cleanup) removes 2026-11-20 from `held_days`: change this test in
// the same PR (it pins the entry). The archiver still waits for the
// day's export after the entry is gone (a valid `.done` keeps the wait).
#[test]
fn held_day_paths_config_holds_2026_11_20_without_disabling_the_archive() {
    let base = read("config/base.toml");
    let section = body(&base, "[partition_retention]", "\n[");
    assert!(
        section.contains("held_days = [\"2026-11-20\"]"),
        "config/base.toml must hold 2026-11-20"
    );
    // Holding by turning the archive off would DROP partitions without a
    // copy (the legacy detach path): the hold must never be done that way.
    assert!(section.contains("archive_enabled = true"));
    assert!(!section.contains("archive_enabled = false"));
}

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tv-held-day-paths-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        Self(dir)
    }
    fn path(&self, sub: &str) -> PathBuf {
        self.0.join(sub)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What each path would do with `day` under `held` (the public deciders the
/// production call sites use, pinned by the scans above).
fn paths_take(held: &HeldDays, day: NaiveDate, today: NaiveDate) -> [bool; 3] {
    let archive = ["ticks", "tick_quotes", "feed_aux_packets", "candles_1m"]
        .iter()
        .all(|t| !held.holds_partition(t, &format!("{}T10", day.format("%Y-%m-%d"))));
    let export = !held.is_held(day);
    let cross_check = tickvault_app::dhan_live_crossverify_boot::past_days_to_check(
        today,
        |_| true,
        |_| false,
        |_| false,
        |x| held.is_held(x),
    )
    .contains(&day);
    [archive, export, cross_check]
}

#[test]
fn test_regression_held_day_skipped_by_all_three_paths() {
    let s = Scratch::new();
    let day = d("2026-11-20");
    let monday = d("2026-11-23");
    // Today pinned after the `.done` written below (a later `.done` releases
    // nothing).
    let today = d("2026-11-30");
    let held =
        HeldDays::with_dirs(vec![day], s.path("held-days"), s.path("day-repair")).with_today(today);
    // Held by config: no path takes it; depth still archives.
    assert_eq!(paths_take(&held, day, monday), [false, false, false]);
    assert!(!held.holds_partition("market_depth_book", "2026-11-20T10"));
    assert!(!held.holds_partition("market_depth", "2026-11-20T10"));
    // Another day is untouched.
    assert_eq!(
        paths_take(&held, d("2026-11-21"), monday),
        [true, true, true]
    );
    // An auto-held day (a `.hold` file, no config entry) is held the same way.
    let unlisted = HeldDays::with_dirs(Vec::new(), s.path("held-days"), s.path("day-repair"))
        .with_today(today);
    write_day_marker(
        &s.path("held-days"),
        MarkerKind::Hold,
        day,
        day,
        "feed time page",
    )
    .expect("hold");
    assert_eq!(paths_take(&unlisted, day, monday), [false, false, false]);
    // A verified repair's `.done` releases it on every path.
    write_day_marker(
        &s.path("day-repair"),
        MarkerKind::Done,
        day,
        d("2026-11-24"),
        "verified",
    )
    .expect("done");
    assert_eq!(paths_take(&held, day, monday), [true, true, true]);
    assert_eq!(paths_take(&unlisted, day, monday), [true, true, true]);
    // The export counts the released day's window from the `.done` date.
    assert_eq!(held.released_days(), vec![(day, d("2026-11-24"))]);
    assert!(
        tickvault_app::brutex_export_boot::visit_order_with_released(
            d("2026-11-30"),
            &held.released_days()
        )
        .contains(&day)
    );
    // With the export on (production), the ARCHIVER waits for the export to
    // seal the released day (`.exported`), so the repaired hours reach S3 in
    // order; the export and the cross-check are released at once.
    let export_first = held.clone().with_export_first(true);
    assert_eq!(paths_take(&export_first, day, monday), [false, true, true]);
    assert!(export_first.note_exported(day).expect("exported"));
    assert_eq!(paths_take(&export_first, day, monday), [true, true, true]);
    assert!(Path::new(&s.path("day-repair")).exists());
}
