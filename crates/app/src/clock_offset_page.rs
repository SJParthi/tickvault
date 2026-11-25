//! The clock-offset page (plan item LF-6a; noise lock §2.22, Quote 57, owner
//! 2026-11-12: "Page if 5 min in a row (Recommended)").
//!
//! The clock-skew poller (`infra::run_clock_skew_loop`) reads `chronyc
//! tracking` every 30 s. The first reading of each minute is one minute
//! sample for [`ClockOffsetEpisode`], a pure state machine:
//!
//! | Sample | Effect |
//! |---|---|
//! | `|offset|` over 1 ms, inside 09:00–15:40 IST | count + 1; the 5th pages ONCE and latches |
//! | `|offset|` at or under 1 ms | count and latch reset, silently (no recovery line) |
//! | failed sample | neither counts nor resets; a coded `warn!` (first failure, then powers of two) |
//! | any sample outside 09:00–15:40 IST | everything resets, nothing logged |
//!
//! The page is one coded `error!` (`CLOCK-SKEW-01`), which the CloudWatch
//! filter `errcode-clock-skew-01` matches on `level = ERROR`; the failed
//! sample `warn!` carries the same code and never matches it.
//!
//! O(1) per sample, no allocation, once a minute on the poller's own task.
//!
//! A sample is built from the RAW chrony reading
//! ([`clock_offset_sample_from_secs`]): any finite offset counts, however
//! large (saturated to `i64` nanoseconds); only a non-finite reading is a
//! failed sample. The one-hour bound applies to the stored minute column
//! only, never to the page, so the largest skews page too.
//!
//! **The latch survives a restart** (§2.22: once per episode). When the page
//! fires, the IST date is written to `data/state/clock-offset-page.state`
//! (temp file, `sync_all`, rename, folder sync); when the episode ends (a
//! reading at or under 1 ms, or the session end) the file is removed. A
//! process that starts with today's latched file starts latched, so the same
//! unbroken episode does not page again; a file from another date is ignored
//! and removed. A failed read or write never pages and never stops the
//! poller: it is a coded `warn!` (same code, `level = WARN`, which the page
//! filter does not match), and a lost latch fails toward one extra page.
//!
//! **Honest limits.** A minute's sample is the FIRST reading in it, so a
//! failure followed by a good reading in the same minute counts as failed.
//! The episode COUNT lives in the poll loop: if the loop dies and is
//! respawned before a page, the count starts again (a page can come up to 5
//! minutes later than it would have). The window is the clock's own
//! 09:00–15:40 IST, judged on the clock being measured; an offset large
//! enough to move the window edge is far over the page threshold anyway.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use tracing::{error, warn};

use tickvault_common::error_code::ErrorCode;
use tickvault_storage::fresh_start_reset::ist_secs_of_day;
use tickvault_storage::hot_path_telemetry::is_in_session_secs_of_day;

/// An absolute offset OVER this many nanoseconds counts (exactly 1 ms does not).
pub const CLOCK_OFFSET_PAGE_THRESHOLD_NS: u64 = 1_000_000;

/// Consecutive over-threshold minute samples that page.
pub const CLOCK_OFFSET_PAGE_CONSECUTIVE_MINUTES: u32 = 5;

/// One minute's clock reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockOffsetSample {
    /// chrony's signed offset in ns (positive = this clock ahead).
    Offset(i64),
    /// The probe failed (no chrony, not synchronised, unreadable).
    Failed,
}

/// What the caller logs for one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockOffsetVerdict {
    /// Nothing to log.
    Quiet,
    /// This minute was already sampled; the reading was not used.
    SameMinute,
    /// Page now (once per episode).
    Page {
        /// The offset that completed the run, ns.
        offset_ns: i64,
        /// Consecutive over-threshold minutes, at least 5.
        consecutive_minutes: u32,
    },
    /// A failed sample inside the window.
    SampleFailed {
        /// Failed samples in a row inside the window.
        consecutive_failures: u64,
    },
}

/// The §2.22 episode state. `Copy`, a few words.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockOffsetEpisode {
    last_minute: Option<i64>,
    over_minutes: u32,
    latched: bool,
    failures: u64,
}

impl ClockOffsetEpisode {
    /// A fresh episode: nothing counted, not latched.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_minute: None,
            over_minutes: 0,
            latched: false,
            failures: 0,
        }
    }

    /// A fresh episode that starts latched when `latched` (today's episode
    /// already paged before a restart), so it does not page again until it
    /// ends. Nothing counted either way.
    #[must_use]
    pub const fn with_latch(latched: bool) -> Self {
        Self {
            last_minute: None,
            over_minutes: 0,
            latched,
            failures: 0,
        }
    }

    /// True while an episode has paged and not yet ended.
    #[must_use]
    pub const fn is_latched(&self) -> bool {
        self.latched
    }

    /// Feeds one minute sample. `unix_minute` identifies the minute (a
    /// second reading in the same minute returns [`ClockOffsetVerdict::SameMinute`]
    /// and changes nothing); `ist_secs_of_day` places it in or out of the
    /// 09:00–15:40 IST window. O(1), pure.
    pub fn observe(
        &mut self,
        unix_minute: i64,
        ist_secs_of_day: u32,
        sample: ClockOffsetSample,
    ) -> ClockOffsetVerdict {
        if self.last_minute == Some(unix_minute) {
            return ClockOffsetVerdict::SameMinute;
        }
        self.last_minute = Some(unix_minute);
        if !is_in_session_secs_of_day(ist_secs_of_day) {
            // Outside the window nothing pages, and the session end closes
            // any episode silently.
            self.over_minutes = 0;
            self.latched = false;
            self.failures = 0;
            return ClockOffsetVerdict::Quiet;
        }
        match sample {
            ClockOffsetSample::Failed => {
                self.failures = self.failures.saturating_add(1);
                ClockOffsetVerdict::SampleFailed {
                    consecutive_failures: self.failures,
                }
            }
            ClockOffsetSample::Offset(ns) => {
                self.failures = 0;
                if ns.unsigned_abs() <= CLOCK_OFFSET_PAGE_THRESHOLD_NS {
                    self.over_minutes = 0;
                    self.latched = false;
                    return ClockOffsetVerdict::Quiet;
                }
                self.over_minutes = self.over_minutes.saturating_add(1);
                if self.over_minutes >= CLOCK_OFFSET_PAGE_CONSECUTIVE_MINUTES && !self.latched {
                    self.latched = true;
                    return ClockOffsetVerdict::Page {
                        offset_ns: ns,
                        consecutive_minutes: self.over_minutes,
                    };
                }
                ClockOffsetVerdict::Quiet
            }
        }
    }
}

/// Feeds the probe taken at `now_unix_secs` to `episode`: works out its
/// minute and IST second of day with the repo helpers, then
/// [`ClockOffsetEpisode::observe`]. O(1), pure.
pub fn feed_clock_offset_episode(
    episode: &mut ClockOffsetEpisode,
    now_unix_secs: i64,
    sample: ClockOffsetSample,
) -> ClockOffsetVerdict {
    episode.observe(
        now_unix_secs.div_euclid(60),
        ist_secs_of_day(now_unix_secs),
        sample,
    )
}

/// One chrony reading in seconds as a minute sample: any finite value is an
/// [`ClockOffsetSample::Offset`] in whole nanoseconds, SATURATED to `i64`
/// (the cast saturates), however large; only a non-finite reading is
/// [`ClockOffsetSample::Failed`]. The one-hour bound of the stored minute
/// column is deliberately NOT applied here, so the largest skews page. Pure.
#[must_use]
pub fn clock_offset_sample_from_secs(offset_secs: f64) -> ClockOffsetSample {
    if !offset_secs.is_finite() {
        return ClockOffsetSample::Failed;
    }
    // `f64 as i64` saturates at the bounds, so this cannot wrap.
    ClockOffsetSample::Offset((offset_secs * 1e9).round() as i64)
}

/// The folder holding the persisted latch (relative to the app working
/// directory, like the other `data/state` files).
pub const CLOCK_OFFSET_LATCH_DIR: &str = "data/state";

/// The persisted latch's file name inside [`CLOCK_OFFSET_LATCH_DIR`].
pub const CLOCK_OFFSET_LATCH_FILE_NAME: &str = "clock-offset-page.state";

/// The most bytes the latch reader looks at (a real file is about 40).
const LATCH_READ_CAP_BYTES: u64 = 256;

fn latch_path_in(dir: &Path) -> PathBuf {
    dir.join(CLOCK_OFFSET_LATCH_FILE_NAME)
}

fn latch_body(date_ist: NaiveDate) -> String {
    format!("date_ist={date_ist}\nlatched=1\n")
}

/// The IST calendar date of `unix_secs`. Pure.
#[must_use]
pub fn ist_date_of_unix_secs(unix_secs: i64) -> NaiveDate {
    let ist = unix_secs.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64);
    chrono::DateTime::from_timestamp(ist, 0)
        .map(|d| d.date_naive())
        .unwrap_or_default()
}

fn warn_latch_io(reason: &'static str, err: &dyn std::fmt::Display) {
    warn!(
        code = ErrorCode::ClockSkew01OffsetOverOneMs.code_str(),
        reason,
        error = %err,
        "clock offset page: the saved episode latch could not be read or written — \
         the poller carries on; at worst one extra page for the same episode"
    );
}

/// Whether today's episode already paged before this process started: reads
/// `<dir>/clock-offset-page.state`. `true` only for a file naming `today_ist`
/// and `latched=1`. A file for another date (or unreadable content) is
/// ignored and removed. A missing file is `false`, silently; a read or
/// remove failure is a coded `warn!` and `false` (never a page, never a
/// panic). Cold: once per poll-loop start.
pub fn restore_clock_offset_latch_in(dir: &Path, today_ist: NaiveDate) -> bool {
    let path = latch_path_in(dir);
    let mut body = String::new();
    match std::fs::File::open(&path) {
        Ok(f) => {
            if let Err(err) = f.take(LATCH_READ_CAP_BYTES).read_to_string(&mut body) {
                warn_latch_io("latch_state_read_failed", &err);
                return false;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            warn_latch_io("latch_state_read_failed", &err);
            return false;
        }
    }
    if body == latch_body(today_ist) {
        return true;
    }
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        warn_latch_io("latch_state_remove_failed", &err);
    }
    false
}

fn write_latch_in(dir: &Path, date_ist: NaiveDate) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{CLOCK_OFFSET_LATCH_FILE_NAME}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(latch_body(date_ist).as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, latch_path_in(dir))?;
    std::fs::File::open(dir)?.sync_all()
}

/// Keeps the saved latch in step with the episode: writes it (dated
/// `date_ist`) when `before` was unlatched and `after` is latched (the page
/// just fired), removes it when `before` was latched and `after` is not (the
/// episode ended: a reading at or under 1 ms, or the session end). Nothing
/// otherwise. A failure is a coded `warn!`, never a page or a panic. Returns
/// `true` when a file operation was attempted. Cold: at most twice per
/// episode.
pub fn persist_clock_offset_latch_change_in(
    dir: &Path,
    date_ist: NaiveDate,
    before: bool,
    after: bool,
) -> bool {
    match (before, after) {
        (false, true) => {
            if let Err(err) = write_latch_in(dir, date_ist) {
                warn_latch_io("latch_state_write_failed", &err);
            }
            true
        }
        (true, false) => {
            if let Err(err) = std::fs::remove_file(latch_path_in(dir))
                && err.kind() != std::io::ErrorKind::NotFound
            {
                warn_latch_io("latch_state_remove_failed", &err);
            }
            true
        }
        _ => false,
    }
}

/// Writes the line a verdict asks for: the coded page `error!`, or the coded
/// `warn!` for a failed sample (first failure, then powers of two). Nothing
/// for the quiet verdicts. Returns `true` when a line was written.
pub fn report_clock_offset_verdict(verdict: ClockOffsetVerdict) -> bool {
    match verdict {
        ClockOffsetVerdict::Page {
            offset_ns,
            consecutive_minutes,
        } => {
            error!(
                code = ErrorCode::ClockSkew01OffsetOverOneMs.code_str(),
                offset_ns,
                consecutive_minutes,
                threshold_ns = CLOCK_OFFSET_PAGE_THRESHOLD_NS,
                "the server clock has been more than 1 ms off for {consecutive_minutes} \
                 minutes in a row in market hours — signal and order times read from it \
                 are off by about offset_ns (positive = clock ahead)"
            );
            true
        }
        ClockOffsetVerdict::SampleFailed {
            consecutive_failures,
        } if consecutive_failures.is_power_of_two() => {
            warn!(
                code = ErrorCode::ClockSkew01OffsetOverOneMs.code_str(),
                reason = "sample_failed",
                consecutive_failures,
                "clock offset sample unavailable in market hours — this minute neither \
                 counts toward nor ends a 1 ms episode"
            );
            true
        }
        ClockOffsetVerdict::SampleFailed { .. }
        | ClockOffsetVerdict::Quiet
        | ClockOffsetVerdict::SameMinute => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10:00 IST, inside the window.
    const IN: u32 = 10 * 3_600;
    /// 16:00 IST, after it.
    const OUT: u32 = 16 * 3_600;
    const HIGH: ClockOffsetSample = ClockOffsetSample::Offset(1_500_000);
    const LOW: ClockOffsetSample = ClockOffsetSample::Offset(-400_000);

    fn run(
        ep: &mut ClockOffsetEpisode,
        start: i64,
        secs: u32,
        samples: &[ClockOffsetSample],
    ) -> Vec<ClockOffsetVerdict> {
        samples
            .iter()
            .enumerate()
            .map(|(i, s)| ep.observe(start + i as i64, secs, *s))
            .collect()
    }

    fn pages(v: &[ClockOffsetVerdict]) -> usize {
        v.iter()
            .filter(|x| matches!(x, ClockOffsetVerdict::Page { .. }))
            .count()
    }

    #[test]
    fn test_observe_four_high_then_ok_never_pages() {
        let mut ep = ClockOffsetEpisode::new();
        let v = run(
            &mut ep,
            0,
            IN,
            &[HIGH, HIGH, HIGH, HIGH, LOW, HIGH, HIGH, HIGH, HIGH],
        );
        assert_eq!(pages(&v), 0, "{v:?}");
        assert!(!ep.is_latched());
    }

    #[test]
    fn test_observe_five_high_pages_once_on_the_fifth() {
        let mut ep = ClockOffsetEpisode::new();
        let v = run(&mut ep, 0, IN, &[HIGH; 5]);
        assert_eq!(pages(&v), 1);
        assert_eq!(
            v[4],
            ClockOffsetVerdict::Page {
                offset_ns: 1_500_000,
                consecutive_minutes: 5
            }
        );
        assert!(v[..4].iter().all(|x| *x == ClockOffsetVerdict::Quiet));
        assert!(ep.is_latched());
    }

    #[test]
    fn test_observe_ten_high_still_pages_once() {
        let mut ep = ClockOffsetEpisode::new();
        let v = run(&mut ep, 0, IN, &[HIGH; 10]);
        assert_eq!(pages(&v), 1);
    }

    #[test]
    fn test_observe_negative_offsets_count_by_magnitude() {
        let mut ep = ClockOffsetEpisode::new();
        let v = run(&mut ep, 0, IN, &[ClockOffsetSample::Offset(-1_000_001); 5]);
        assert_eq!(pages(&v), 1);
    }

    #[test]
    fn test_observe_exactly_one_ms_does_not_count() {
        let mut ep = ClockOffsetEpisode::new();
        let mut samples = vec![HIGH; 4];
        samples.push(ClockOffsetSample::Offset(1_000_000));
        samples.extend([HIGH; 4]);
        assert_eq!(pages(&run(&mut ep, 0, IN, &samples)), 0);
        let mut ep = ClockOffsetEpisode::new();
        assert_eq!(
            pages(&run(
                &mut ep,
                0,
                IN,
                &[ClockOffsetSample::Offset(i64::MIN); 5]
            )),
            1,
            "i64::MIN does not overflow the magnitude"
        );
    }

    #[test]
    fn test_observe_failed_sample_in_the_middle_neither_counts_nor_resets() {
        let mut ep = ClockOffsetEpisode::new();
        let f = ClockOffsetSample::Failed;
        let v = run(&mut ep, 0, IN, &[HIGH, HIGH, f, f, HIGH, HIGH, HIGH]);
        assert_eq!(pages(&v), 1, "{v:?}");
        assert!(matches!(
            v[6],
            ClockOffsetVerdict::Page {
                consecutive_minutes: 5,
                ..
            }
        ));
        assert_eq!(
            v[2],
            ClockOffsetVerdict::SampleFailed {
                consecutive_failures: 1
            }
        );
        assert_eq!(
            v[3],
            ClockOffsetVerdict::SampleFailed {
                consecutive_failures: 2
            }
        );
        // While latched a failure does not end the episode either.
        let v = run(&mut ep, 100, IN, &[f, HIGH, HIGH]);
        assert_eq!(pages(&v), 0);
        assert!(ep.is_latched());
    }

    #[test]
    fn test_observe_outside_hours_never_pages() {
        let mut ep = ClockOffsetEpisode::new();
        let v = run(&mut ep, 0, OUT, &[HIGH; 20]);
        assert_eq!(pages(&v), 0);
        assert!(v.iter().all(|x| *x == ClockOffsetVerdict::Quiet));
        let v = run(
            &mut ep,
            100,
            8 * 3_600 + 59 * 60,
            &[ClockOffsetSample::Failed; 3],
        );
        assert!(
            v.iter().all(|x| *x == ClockOffsetVerdict::Quiet),
            "no warn before 09:00"
        );
    }

    #[test]
    fn test_observe_window_edges_are_09_00_inclusive_and_15_40_exclusive() {
        let mut ep = ClockOffsetEpisode::new();
        assert_eq!(pages(&run(&mut ep, 0, 9 * 3_600, &[HIGH; 5])), 1);
        let mut ep = ClockOffsetEpisode::new();
        assert_eq!(pages(&run(&mut ep, 0, 15 * 3_600 + 40 * 60, &[HIGH; 5])), 0);
    }

    #[test]
    fn test_observe_session_end_resets_the_episode() {
        let mut ep = ClockOffsetEpisode::new();
        assert_eq!(pages(&run(&mut ep, 0, IN, &[HIGH; 5])), 1);
        // The session ends (15:40), the next day starts over: four highs
        // after the reset are not enough, five page again.
        let _ = run(&mut ep, 10, OUT, &[HIGH]);
        assert!(!ep.is_latched());
        assert_eq!(pages(&run(&mut ep, 20, IN, &[HIGH; 4])), 0);
        assert_eq!(pages(&run(&mut ep, 24, IN, &[HIGH])), 1);
        // Two highs before the close plus three after the open never join.
        let mut ep = ClockOffsetEpisode::new();
        let _ = run(&mut ep, 0, IN, &[HIGH; 2]);
        let _ = run(&mut ep, 2, OUT, &[LOW]);
        assert_eq!(pages(&run(&mut ep, 3, IN, &[HIGH; 3])), 0);
    }

    #[test]
    fn test_observe_recovery_is_silent_and_rearms() {
        let mut ep = ClockOffsetEpisode::new();
        let v = run(&mut ep, 0, IN, &[HIGH, HIGH, HIGH, HIGH, HIGH, LOW]);
        assert_eq!(v[5], ClockOffsetVerdict::Quiet, "no recovery line");
        assert!(!ep.is_latched());
        assert_eq!(
            pages(&run(&mut ep, 10, IN, &[HIGH; 5])),
            1,
            "a new episode pages again"
        );
    }

    #[test]
    fn test_observe_same_minute_is_ignored() {
        let mut ep = ClockOffsetEpisode::new();
        for _ in 0..10 {
            let _ = ep.observe(7, IN, HIGH);
        }
        assert_eq!(ep.observe(7, IN, HIGH), ClockOffsetVerdict::SameMinute);
        assert_eq!(
            pages(&run(&mut ep, 8, IN, &[HIGH; 3])),
            0,
            "only 4 minutes counted"
        );
        assert_eq!(pages(&run(&mut ep, 11, IN, &[HIGH])), 1);
    }

    #[test]
    fn test_new_and_default_are_the_same_fresh_episode() {
        assert_eq!(ClockOffsetEpisode::new(), ClockOffsetEpisode::default());
        assert!(!ClockOffsetEpisode::new().is_latched());
    }

    #[test]
    fn test_is_latched_follows_the_episode() {
        let mut ep = ClockOffsetEpisode::new();
        let _ = run(&mut ep, 0, IN, &[HIGH; 5]);
        assert!(ep.is_latched());
        let _ = ep.observe(9, IN, LOW);
        assert!(!ep.is_latched());
    }

    #[test]
    fn test_feed_clock_offset_episode_uses_the_ist_minute_and_window() {
        // 2026-11-12 10:00:00 IST = 04:30:00 UTC.
        let ten_ist = 1_794_457_800;
        let mut ep = ClockOffsetEpisode::new();
        let mut verdicts = Vec::new();
        for m in 0..5 {
            // Two probes a minute, 30 s apart, like the poller.
            verdicts.push(feed_clock_offset_episode(&mut ep, ten_ist + m * 60, HIGH));
            verdicts.push(feed_clock_offset_episode(
                &mut ep,
                ten_ist + m * 60 + 30,
                HIGH,
            ));
        }
        assert_eq!(pages(&verdicts), 1);
        assert_eq!(verdicts[1], ClockOffsetVerdict::SameMinute);
        // 16:00 IST: outside, resets.
        let four_pm_ist = ten_ist + 6 * 3_600;
        assert_eq!(
            feed_clock_offset_episode(&mut ep, four_pm_ist, HIGH),
            ClockOffsetVerdict::Quiet
        );
        assert!(!ep.is_latched());
    }

    #[test]
    fn test_report_clock_offset_verdict_writes_only_pages_and_throttled_failures() {
        assert!(report_clock_offset_verdict(ClockOffsetVerdict::Page {
            offset_ns: 2_000_000,
            consecutive_minutes: 5
        }));
        for (n, logged) in [
            (1, true),
            (2, true),
            (3, false),
            (4, true),
            (5, false),
            (8, true),
        ] {
            assert_eq!(
                report_clock_offset_verdict(ClockOffsetVerdict::SampleFailed {
                    consecutive_failures: n
                }),
                logged,
                "failure {n}"
            );
        }
        assert!(!report_clock_offset_verdict(ClockOffsetVerdict::Quiet));
        assert!(!report_clock_offset_verdict(ClockOffsetVerdict::SameMinute));
    }

    /// Review finding (LF-6a): a reading over one hour became a failed
    /// sample through the stored column's bound, so the largest skews never
    /// paged. Any finite reading now counts, saturated to `i64`.
    #[test]
    fn test_regression_huge_offsets_are_samples_and_page() {
        assert_eq!(
            clock_offset_sample_from_secs(7_200.0),
            ClockOffsetSample::Offset(7_200_000_000_000)
        );
        assert_eq!(
            clock_offset_sample_from_secs(-1e30),
            ClockOffsetSample::Offset(i64::MIN)
        );
        assert_eq!(
            clock_offset_sample_from_secs(1e30),
            ClockOffsetSample::Offset(i64::MAX)
        );
        let mut ep = ClockOffsetEpisode::new();
        let samples = [clock_offset_sample_from_secs(7_200.0); 5];
        assert_eq!(pages(&run(&mut ep, 0, IN, &samples)), 1);
    }

    #[test]
    fn test_clock_offset_sample_from_secs_only_non_finite_fails() {
        assert_eq!(
            clock_offset_sample_from_secs(0.0015),
            ClockOffsetSample::Offset(1_500_000)
        );
        assert_eq!(
            clock_offset_sample_from_secs(-0.0004),
            ClockOffsetSample::Offset(-400_000)
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                clock_offset_sample_from_secs(bad),
                ClockOffsetSample::Failed
            );
        }
    }

    fn latch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-clock-latch-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).expect("date")
    }

    /// One process's run of minute samples, as the poll loop drives it:
    /// restore the latch at start, keep the file in step after each sample.
    fn process_run(
        dir: &Path,
        today: NaiveDate,
        start: i64,
        samples: &[ClockOffsetSample],
    ) -> usize {
        let mut ep = ClockOffsetEpisode::with_latch(restore_clock_offset_latch_in(dir, today));
        let mut paged = 0;
        for (i, s) in samples.iter().enumerate() {
            let before = ep.is_latched();
            if matches!(
                ep.observe(start + i as i64, IN, *s),
                ClockOffsetVerdict::Page { .. }
            ) {
                paged += 1;
            }
            persist_clock_offset_latch_change_in(dir, today, before, ep.is_latched());
        }
        paged
    }

    /// Review finding (LF-6a, noise lock §2.22): a restart in the middle of
    /// an unbroken episode paged it a second time.
    #[test]
    fn test_regression_restart_while_latched_does_not_page_again() {
        let dir = latch_dir("restart");
        assert_eq!(process_run(&dir, day(12), 0, &[HIGH; 6]), 1);
        assert!(latch_path_in(&dir).exists(), "the page saves the latch");
        // The process restarts; the episode carries on, unbroken.
        assert_eq!(process_run(&dir, day(12), 100, &[HIGH; 10]), 0);
        assert!(latch_path_in(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restore_clock_offset_latch_in_ignores_and_removes_another_day() {
        let dir = latch_dir("newday");
        assert_eq!(process_run(&dir, day(12), 0, &[HIGH; 5]), 1);
        assert!(!restore_clock_offset_latch_in(&dir, day(13)));
        assert!(!latch_path_in(&dir).exists(), "a stale file is removed");
        assert_eq!(
            process_run(&dir, day(13), 100, &[HIGH; 5]),
            1,
            "a new day pages again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_persist_clock_offset_latch_change_in_recovery_removes_and_rearms() {
        let dir = latch_dir("recover");
        assert_eq!(
            process_run(&dir, day(12), 0, &[HIGH, HIGH, HIGH, HIGH, HIGH, LOW]),
            1
        );
        assert!(!latch_path_in(&dir).exists(), "the episode end removes it");
        assert_eq!(process_run(&dir, day(12), 100, &[HIGH; 5]), 1);
        // Session end also ends it and removes the file.
        let mut ep = ClockOffsetEpisode::with_latch(restore_clock_offset_latch_in(&dir, day(12)));
        assert!(ep.is_latched());
        let _ = ep.observe(500, OUT, HIGH);
        assert!(persist_clock_offset_latch_change_in(
            &dir,
            day(12),
            true,
            ep.is_latched()
        ));
        assert!(!latch_path_in(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_latch_io_failures_never_panic_and_read_as_unlatched() {
        // Missing folder: unlatched, silently.
        let dir = latch_dir("missing");
        assert!(!restore_clock_offset_latch_in(&dir, day(12)));
        // Garbage content: ignored and removed.
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(latch_path_in(&dir), "nonsense").expect("write");
        assert!(!restore_clock_offset_latch_in(&dir, day(12)));
        assert!(!latch_path_in(&dir).exists());
        // A folder where the file should be: read fails, a warn, unlatched.
        std::fs::create_dir_all(latch_path_in(&dir)).expect("dir as file");
        assert!(!restore_clock_offset_latch_in(&dir, day(12)));
        // The write target's parent is a FILE: the write fails, no panic.
        let blocked = dir.join("blocker");
        std::fs::write(&blocked, "x").expect("write");
        assert!(persist_clock_offset_latch_change_in(
            &blocked,
            day(12),
            false,
            true
        ));
        // No change: no file operation.
        assert!(!persist_clock_offset_latch_change_in(
            &dir,
            day(12),
            true,
            true
        ));
        assert!(!persist_clock_offset_latch_change_in(
            &dir,
            day(12),
            false,
            false
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ist_date_of_unix_secs_and_with_latch() {
        assert!(ClockOffsetEpisode::with_latch(true).is_latched());
        assert_eq!(
            ClockOffsetEpisode::with_latch(false),
            ClockOffsetEpisode::new()
        );
        // 2026-11-12 18:30 UTC is 2026-11-13 00:00 IST.
        assert_eq!(ist_date_of_unix_secs(1_794_508_200), day(13));
        assert_eq!(ist_date_of_unix_secs(1_794_508_199), day(12));
        assert_eq!(
            CLOCK_OFFSET_LATCH_DIR, "data/state",
            "beside the other data/state files"
        );
    }

    #[test]
    fn test_threshold_constants_are_the_rule_values() {
        // Noise lock §2.22: over 1 ms, 5 consecutive minutes.
        assert_eq!(CLOCK_OFFSET_PAGE_THRESHOLD_NS, 1_000_000);
        assert_eq!(CLOCK_OFFSET_PAGE_CONSECUTIVE_MINUTES, 5);
    }

    /// The live poll loop feeds every probe to the episode and logs its
    /// verdict, and the probe publishes the reading for the minute column.
    #[test]
    fn test_clock_skew_poll_loop_drives_the_episode_and_the_column() {
        let infra = include_str!("infra.rs");
        let start = infra
            .find("async fn run_clock_skew_loop(")
            .expect("run_clock_skew_loop");
        let body = &infra[start..];
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(body.contains("ClockOffsetEpisode::with_latch(restored)"));
        assert!(body.contains("restore_clock_offset_latch_in("));
        assert!(body.contains("persist_clock_offset_latch_change_in("));
        assert!(body.contains("clock_offset_sample_from_secs(s.skew_secs)"));
        assert!(
            !body.contains("clock_offset_ns_from_secs("),
            "the page must not take the stored column's one-hour bound"
        );
        assert!(body.contains("feed_clock_offset_episode("));
        assert!(body.contains("report_clock_offset_verdict("));
        let probe = &infra[infra
            .find("async fn probe_clock_skew_using")
            .expect("probe")..];
        let probe = &probe[..probe.find("\n}\n").expect("end of fn")];
        assert_eq!(
            probe.matches("publish_clock_offset(").count(),
            2,
            "Ok and Err arms"
        );
    }
}
