//! Holiday-calendar coverage-horizon staleness watchdog (W2 PR#5,
//! 2026-07-10 — 2026-07-09 audit follow-up row 15).
//!
//! ## The gap this closes
//!
//! `config/base.toml [[trading.nse_holidays]]` covers ONE calendar year at
//! a time (2026 today), and `TradingCalendar::is_holiday` is a bare set
//! lookup with NO year bound — so the moment the year rolls past the
//! newest listed holiday, every un-listed weekday holiday silently reads
//! as a trading day: the box auto-starts, both feeds connect, and a full
//! billable session burns on a market-closed day (false "market open").
//! Before this watchdog, the ONLY detection was two build-time ratchets
//! (`crates/common/tests/nse_holiday_calendar_guard.rs` +
//! `nse_holiday_calendar_currency_guard.rs`) that go RED on/after Jan 1 —
//! reactive, CI-only, with zero runtime operator signal as the cliff
//! APPROACHES.
//!
//! ## The mechanism (config-only input, no network)
//!
//! Coverage horizon = Dec 31 of the newest configured holiday year
//! (`TradingCalendar::coverage_end_date` — NSE circulars are per calendar
//! year). When fewer than
//! [`CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS`](tickvault_common::constants::CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS)
//! days remain (strict `<`), the watchdog may fire
//! `NotificationEvent::HolidayCalendarCoverageLow` (Severity::High) on the
//! cadence below (audit Rule 4 — edge-latched, never per tick), sets the
//! `tv_calendar_coverage_days_remaining` gauge on every check, and
//! increments `tv_calendar_staleness_alerts_total` per fired page.
//!
//! ## Check cadence
//!
//! Checks immediately at spawn (every boot), then every 6h. The AWS box
//! restarts every trading weekday at 08:30 IST and is OFF at IST midnight,
//! so a midnight-anchored task would NEVER run in prod — the boot-time
//! check is the prod daily cadence; the 6h loop covers long-lived local
//! sessions. Deliberately NOT market-hours-gated: this is date-based
//! calendar maintenance (Rule 3 targets market-data tasks that false-page
//! off-hours; a stale calendar matters most across the year-end holiday
//! stretch).
//!
//! ## Page cadence (plan PR AL-6, 2026-11-06)
//!
//! Owner, 2026-11-06: *"why im keepm gettign so many issues and errors
//! ntoficatiosn always dude why dude hwy fix adn reosovle evruthgin all these
//! alsod dude"*. With the 60-day threshold the page fired on every boot from
//! about 1 November, weeks before anything was urgent. Now:
//!
//! | Coverage left | Page |
//! |---|---|
//! | 60 days or more | never (gauge only) |
//! | more than [`WEEKLY_PAGE_ABOVE_DAYS`] (14), under 60 | at most once per ISO week: the first check on or after Monday |
//! | 14 or fewer, already run out, or empty calendar | every boot, as before (once per process per IST day) |
//!
//! The ISO week last paged is kept in [`CALENDAR_STALENESS_WEEK_PATH`] (one
//! line, `iso_week=<year>-W<week>`; temp file, `sync_all`, rename, folder
//! sync; one small read at start and one write per page, O(1), on the
//! blocking pool, cold). The gauge is still set on every check.
//!
//! **Honest limits.** A missing or damaged file, a path that is not a
//! regular file, or a read slower than 2 s reads as "not paged this week",
//! so it pages (errs toward paging). The temp file is created exclusively,
//! owner read/write only on unix. A failed write logs one `warn!`
//! and the next boot pages again. A wall clock stepped back into an earlier
//! week pages once for that week. The per-day latch inside a process is
//! still in memory, so in the every-boot band a restart storm pages once per
//! restart (hostile-review M1, 2026-07-10, accepted envelope; the storm
//! itself already pages louder than this reminder).
//!
//! ## Deliberately NOT wired here (follow-up, operator quote required)
//!
//! The dormant NSE-endpoint cross-check helpers
//! (`crates/core/src/instrument/nse_holiday_cross_check.rs`) stay dormant:
//! wiring them needs a NEW nseindia.com REST fetch + cookie-warmup, gated
//! by `no-rest-except-live-feed-2026-06-27.md`'s edit-rule-file-first
//! protocol. This watchdog needs no network at all.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use chrono::{Datelike, NaiveDate};
use tickvault_common::constants::{
    CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS, CALENDAR_STALENESS_CHECK_INTERVAL_SECS,
    CALENDAR_STALENESS_NOTIFIER_RETRY_SECS,
};
use tickvault_common::trading_calendar::{TradingCalendar, is_calendar_coverage_stale, ist_offset};
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tracing::{info, warn};

/// Gauge: signed days of holiday-calendar coverage remaining (set on every
/// check; absent only when the calendar is empty — no fake 0).
const GAUGE_COVERAGE_DAYS_REMAINING: &str = "tv_calendar_coverage_days_remaining";

/// Counter: one increment per fired staleness Telegram page.
const COUNTER_STALENESS_ALERTS: &str = "tv_calendar_staleness_alerts_total";

/// Above this many days of coverage left, the page fires at most once per
/// ISO week; at or below it (or run out, or empty), every boot (plan PR AL-6).
pub const WEEKLY_PAGE_ABOVE_DAYS: i64 = 14;

/// Where the ISO week last paged lives, relative to the app working
/// directory like the sibling `data/state` files.
pub const CALENDAR_STALENESS_WEEK_PATH: &str = "data/state/calendar-staleness-week";

/// The line prefix of the week file.
const WEEK_PREFIX: &str = "iso_week=";

/// The most bytes the reader looks at; a real file is about 18.
const WEEK_READ_CAP_BYTES: u64 = 64;

/// The start-up read of the week file gives up after this long (a stalled
/// volume); a timeout reads as "not paged this week" (errs toward paging).
const WEEK_READ_TIMEOUT_SECS: u64 = 2;

/// An ISO week, `(iso year, week number)`.
pub type IsoWeekKey = (i32, u32);

/// The ISO week `day` falls in (weeks start on Monday). O(1), pure.
#[must_use]
pub fn iso_week_of(day: NaiveDate) -> IsoWeekKey {
    let week = day.iso_week();
    (week.year(), week.week())
}

/// Pure per-check decision: should THIS check page?
///
/// Never when coverage is not stale (per [`is_calendar_coverage_stale`]),
/// and never twice in one process on one IST day (`last_alerted`, audit
/// Rule 4). Past those: with more than [`WEEKLY_PAGE_ABOVE_DAYS`] left it
/// pages only when `last_paged_week` is not `today`'s ISO week; with that
/// many or fewer left, already run out, or an empty calendar it pages.
#[must_use]
pub fn should_page_now(
    days_remaining: Option<i64>,
    threshold_days: i64,
    today: NaiveDate,
    last_alerted: Option<NaiveDate>,
    last_paged_week: Option<IsoWeekKey>,
) -> bool {
    if !is_calendar_coverage_stale(days_remaining, threshold_days) || last_alerted == Some(today) {
        return false;
    }
    let weekly_band = days_remaining.is_some_and(|days| days > WEEKLY_PAGE_ABOVE_DAYS);
    !weekly_band || last_paged_week != Some(iso_week_of(today))
}

/// The ISO week last paged, or `None` when the file is missing, unreadable
/// or not exactly one valid line. Cold, one small read.
#[must_use]
pub fn read_last_paged_week(path: &Path) -> Option<IsoWeekKey> {
    // Only a regular file is read: a FIFO or device at this path could
    // block the read forever (review 2026-11-06; the caller also bounds it
    // with WEEK_READ_TIMEOUT_SECS).
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::with_capacity(32);
    file.take(WEEK_READ_CAP_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let value = text.trim_end_matches('\n').strip_prefix(WEEK_PREFIX)?;
    let (year, week) = value.split_once("-W")?;
    let year = year.parse::<i32>().ok()?;
    let week = week.parse::<u32>().ok()?;
    (1..=53).contains(&week).then_some((year, week))
}

/// Records the ISO week paged: temp file, `sync_all`, rename, folder sync.
///
/// # Errors
/// Any I/O error creating the folder or writing, syncing or renaming the file.
pub fn write_last_paged_week(path: &Path, week: IsoWeekKey) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    // A stale temp file from a crashed write is removed first, then the new
    // one is created exclusively (`create_new`, owner read/write only on
    // unix), so a link planted at the temp path is never followed.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(format!("{WEEK_PREFIX}{}-W{:02}\n", week.0, week.1).as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
}

/// Renders the human "last covered day" for the Telegram body, e.g.
/// `"31 Dec 2026"`. `None` (empty calendar — pathological, config guards
/// prevent it in prod) renders honestly as `"none configured"`.
#[must_use]
pub fn coverage_end_display(coverage_end: Option<NaiveDate>) -> String {
    match coverage_end {
        Some(end) => end.format("%d %b %Y").to_string(),
        None => "none configured".to_string(),
    }
}

/// Spawns the calendar-staleness watchdog: immediate check at boot, then
/// every [`CALENDAR_STALENESS_CHECK_INTERVAL_SECS`].
///
/// `notifier_slot` is the lazily-filled process notifier (the same
/// `ArcSwapOption` slot the Groww sidecar diagnostics use) — the watchdog
/// spawns in the common boot prefix BEFORE the notifier exists, so a stale
/// check with an empty slot retries in
/// [`CALENDAR_STALENESS_NOTIFIER_RETRY_SECS`] without consuming the daily
/// latch (the alert is delayed, never lost for the day).
///
/// The loop body is panic-free by construction (pure date math + gauge set
/// + `ArcSwapOption` load + fire-and-forget `notify`) — no supervisor
/// needed; worst case the next boot re-checks (daily in prod).
// The pure decision core (`should_alert_today` / `is_calendar_coverage_stale`
// / `coverage_end_display`) is fully unit-tested; the main.rs wiring is
// ratcheted by `test_calendar_staleness_watchdog_is_wired_into_main`.
// TEST-EXEMPT: infinite tokio loop (sleep-driven) — pure core + wiring tested
pub fn spawn_calendar_staleness_watchdog(
    calendar: Arc<TradingCalendar>,
    notifier_slot: Arc<ArcSwapOption<NotificationService>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_alerted: Option<NaiveDate> = None;
        let mut last_warned_no_notifier: Option<NaiveDate> = None;
        // Cold, once per process: the ISO week last paged (AL-6). A failed
        // read or a join error reads as "not paged" (errs toward paging).
        let mut last_paged_week: Option<IsoWeekKey> = tokio::time::timeout(
            std::time::Duration::from_secs(WEEK_READ_TIMEOUT_SECS),
            tokio::task::spawn_blocking(|| {
                read_last_paged_week(Path::new(CALENDAR_STALENESS_WEEK_PATH))
            }),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
        info!(
            coverage_end = %coverage_end_display(calendar.coverage_end_date()),
            threshold_days = CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS,
            "calendar-staleness watchdog started"
        );
        loop {
            let today = chrono::Utc::now().with_timezone(&ist_offset()).date_naive();
            let remaining = calendar.coverage_days_remaining(today);
            if let Some(days) = remaining {
                // APPROVED: day counts are tiny (|d| < 10^6) — f64 exact well past 2^53
                #[allow(clippy::cast_precision_loss)]
                metrics::gauge!(GAUGE_COVERAGE_DAYS_REMAINING).set(days as f64);
            }

            if should_page_now(
                remaining,
                CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS,
                today,
                last_alerted,
                last_paged_week,
            ) {
                let Some(notifier) = notifier_slot.load_full() else {
                    // Boot prefix: notifier not built yet — retry shortly
                    // WITHOUT setting the alert latch, so the page is
                    // delayed, never lost for the day. Hostile-review M2
                    // (2026-07-10): a slot that NEVER fills (boot path that
                    // bails before notifier construction) must still be
                    // log-sink-visible — one warn! per IST day, own latch.
                    if last_warned_no_notifier != Some(today) {
                        warn!(
                            days_remaining = ?remaining,
                            "holiday-calendar coverage horizon is low but the \
                             Telegram notifier is not up yet — retrying \
                             delivery every 30s (log-sink-only until then)"
                        );
                        last_warned_no_notifier = Some(today);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(
                        CALENDAR_STALENESS_NOTIFIER_RETRY_SECS,
                    ))
                    .await;
                    continue;
                };
                let end_display = coverage_end_display(calendar.coverage_end_date());
                warn!(
                    days_remaining = ?remaining,
                    coverage_end = %end_display,
                    threshold_days = CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS,
                    "holiday-calendar coverage horizon is low — paging operator \
                     (paste the next official NSE circular into config)"
                );
                notifier.notify(NotificationEvent::HolidayCalendarCoverageLow {
                    days_remaining: remaining,
                    coverage_end_display: end_display,
                });
                metrics::counter!(COUNTER_STALENESS_ALERTS).increment(1);
                last_alerted = Some(today);
                let week = iso_week_of(today);
                last_paged_week = Some(week);
                let written = tokio::task::spawn_blocking(move || {
                    write_last_paged_week(Path::new(CALENDAR_STALENESS_WEEK_PATH), week)
                })
                .await;
                if !matches!(written, Ok(Ok(()))) {
                    warn!(
                        path = CALENDAR_STALENESS_WEEK_PATH,
                        "could not record the week the calendar reminder was sent — \
                         the next boot may send it again"
                    );
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(
                CALENDAR_STALENESS_CHECK_INTERVAL_SECS,
            ))
            .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap() // APPROVED: test-only literal dates
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!(
                "tv-calendar-week-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ))
            .join("state")
            .join("calendar-staleness-week")
    }

    #[test]
    fn test_should_page_now_daily_latch_and_threshold() {
        let today = ymd(2026, 12, 20); // 11 days left: every-boot band
        // Stale + never alerted → fire.
        assert!(should_page_now(Some(11), 60, today, None, None));
        // Already alerted TODAY in this process → latched.
        assert!(!should_page_now(Some(11), 60, today, Some(today), None));
        // Last alerted YESTERDAY → new IST day, fire again.
        assert!(should_page_now(
            Some(11),
            60,
            today,
            Some(ymd(2026, 12, 19)),
            None
        ));
        // Exactly the threshold (strict `<`) → never fires.
        assert!(!should_page_now(Some(60), 60, today, None, None));
    }

    #[test]
    fn test_more_than_14_days_left_pages_once_per_iso_week() {
        // Fri 6 Nov 2026 (ISO 2026-W45), 55 days left.
        let friday = ymd(2026, 11, 6);
        let this_week = iso_week_of(friday);
        assert_eq!(this_week, (2026, 45));
        assert!(should_page_now(Some(55), 60, friday, None, None));
        // Already paged this ISO week → a new boot stays quiet.
        assert!(!should_page_now(
            Some(55),
            60,
            friday,
            None,
            Some(this_week)
        ));
        // Sunday of the same week → still quiet.
        assert!(!should_page_now(
            Some(53),
            60,
            ymd(2026, 11, 8),
            None,
            Some(this_week)
        ));
        // Monday 9 Nov (W46) → the first check of the new week pages.
        assert!(should_page_now(
            Some(52),
            60,
            ymd(2026, 11, 9),
            None,
            Some(this_week)
        ));
        // 15 days left is still weekly.
        assert!(!should_page_now(
            Some(15),
            60,
            ymd(2026, 12, 16),
            None,
            Some(iso_week_of(ymd(2026, 12, 16)))
        ));
    }

    #[test]
    fn test_14_or_fewer_days_run_out_or_empty_page_every_boot() {
        let today = ymd(2026, 12, 17);
        let paged = Some(iso_week_of(today));
        // A page earlier this week does not hold these back.
        assert!(should_page_now(Some(14), 60, today, None, paged));
        assert!(should_page_now(Some(0), 60, today, None, paged));
        assert!(should_page_now(Some(-5), 60, today, None, paged));
        assert!(should_page_now(None, 60, today, None, paged));
        // The in-process day latch still holds within one process.
        assert!(!should_page_now(Some(14), 60, today, Some(today), paged));
    }

    #[test]
    fn test_iso_week_spans_a_year_boundary() {
        // Thu 31 Dec 2026 and Fri 1 Jan 2027 are both ISO 2026-W53.
        assert_eq!(iso_week_of(ymd(2026, 12, 31)), (2026, 53));
        assert_eq!(iso_week_of(ymd(2027, 1, 1)), (2026, 53));
        assert_eq!(iso_week_of(ymd(2027, 1, 4)), (2027, 1));
    }

    #[test]
    fn test_last_paged_week_round_trips_through_the_file() {
        let path = temp_path("round");
        assert_eq!(read_last_paged_week(&path), None, "missing file reads None");
        write_last_paged_week(&path, (2026, 45)).unwrap(); // APPROVED: test
        assert_eq!(read_last_paged_week(&path), Some((2026, 45)));
        let text = std::fs::read_to_string(&path).unwrap(); // APPROVED: test
        assert_eq!(text, "iso_week=2026-W45\n");
        write_last_paged_week(&path, (2027, 1)).unwrap(); // APPROVED: test
        assert_eq!(read_last_paged_week(&path), Some((2027, 1)));
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file renamed away"
        );
    }

    #[test]
    fn test_a_path_that_is_not_a_regular_file_reads_as_not_paged() {
        // A directory (standing in for a FIFO or device) is never opened.
        let path = temp_path("not-regular");
        std::fs::create_dir_all(&path).unwrap(); // APPROVED: test
        assert_eq!(read_last_paged_week(&path), None);
    }

    #[test]
    fn test_a_stale_temp_file_is_replaced_and_the_file_is_owner_only() {
        let path = temp_path("stale-tmp");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap(); // APPROVED: test
        std::fs::write(path.with_extension("tmp"), b"left by a crash").unwrap(); // APPROVED: test
        write_last_paged_week(&path, (2026, 46)).unwrap(); // APPROVED: test
        assert_eq!(read_last_paged_week(&path), Some((2026, 46)));
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode(); // APPROVED: test
            assert_eq!(mode & 0o777, 0o600, "owner read/write only");
        }
    }

    #[test]
    fn test_a_damaged_week_file_reads_as_not_paged() {
        let path = temp_path("damaged");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap(); // APPROVED: test
        for body in [
            "",
            "garbage\n",
            "iso_week=2026\n",
            "iso_week=2026-W99\n",
            "iso_week=2026-W0\n",
            "iso_week=x-W5\n",
        ] {
            std::fs::write(&path, body).unwrap(); // APPROVED: test
            assert_eq!(read_last_paged_week(&path), None, "{body:?}");
        }
    }

    #[test]
    fn test_an_unwritable_folder_returns_an_error() {
        let path = temp_path("unwritable");
        let parent = path.parent().unwrap().to_path_buf(); // APPROVED: test
        // A FILE where the folder should be makes create_dir_all fail.
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap(); // APPROVED: test
        std::fs::write(&parent, b"x").unwrap(); // APPROVED: test
        assert!(write_last_paged_week(&path, (2026, 45)).is_err());
    }

    #[test]
    fn test_coverage_end_display_formats() {
        assert_eq!(coverage_end_display(Some(ymd(2026, 12, 31))), "31 Dec 2026");
        assert_eq!(coverage_end_display(None), "none configured");
    }
}
