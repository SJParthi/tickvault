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
//! days remain (strict `<`), the watchdog fires ONE
//! `NotificationEvent::HolidayCalendarCoverageLow` (Severity::High) per
//! process per IST day (audit Rule 4 — edge-latched, never per tick), sets
//! the `tv_calendar_coverage_days_remaining` gauge on every check, and
//! increments `tv_calendar_staleness_alerts_total` per fired page.
//!
//! ## Cadence honesty
//!
//! Checks immediately at spawn (every boot), then every 6h. The AWS box
//! restarts every trading weekday at 08:30 IST and is OFF at IST midnight,
//! so a midnight-anchored task would NEVER run in prod — the boot-time
//! check is the prod daily cadence; the 6h loop covers long-lived local
//! sessions. Deliberately NOT market-hours-gated: this is date-based
//! calendar maintenance (Rule 3 targets market-data tasks that false-page
//! off-hours; a stale calendar matters most across the year-end holiday
//! stretch). Honest envelope: the alert latch is in-memory, so a restart
//! re-arms it — at most one page per PROCESS per IST day. On a normal day
//! that is 1-2 pages; a WS-GAP-09-class restart STORM during a stale
//! window would page once per restart (the storm itself already pages
//! louder than this reminder, and the episode machinery + a persistent
//! latch are deliberately NOT added for a maintenance reminder —
//! hostile-review M1, 2026-07-10, accepted envelope).
//!
//! ## Deliberately NOT wired here (follow-up, operator quote required)
//!
//! The dormant NSE-endpoint cross-check helpers
//! (`crates/core/src/instrument/nse_holiday_cross_check.rs`) stay dormant:
//! wiring them needs a NEW nseindia.com REST fetch + cookie-warmup, gated
//! by `no-rest-except-live-feed-2026-06-27.md`'s edit-rule-file-first
//! protocol. This watchdog needs no network at all.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use chrono::NaiveDate;
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

/// Pure per-check decision: should THIS check fire the daily alert?
///
/// True iff the coverage is stale (per
/// [`is_calendar_coverage_stale`]) AND no alert has fired for `today` yet
/// (the per-IST-date latch — audit Rule 4, one page per process per day).
#[must_use]
pub fn should_alert_today(
    days_remaining: Option<i64>,
    threshold_days: i64,
    today: NaiveDate,
    last_alerted: Option<NaiveDate>,
) -> bool {
    is_calendar_coverage_stale(days_remaining, threshold_days) && last_alerted != Some(today)
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

            if should_alert_today(
                remaining,
                CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS,
                today,
                last_alerted,
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

    #[test]
    fn test_should_alert_today_boundaries() {
        let today = ymd(2026, 11, 15);
        // Stale (46 < 60) + never alerted → fire.
        assert!(should_alert_today(Some(46), 60, today, None));
        // Stale + already alerted TODAY → latched, no re-fire.
        assert!(!should_alert_today(Some(46), 60, today, Some(today)));
        // Stale + last alerted YESTERDAY → new IST day, fire again.
        assert!(should_alert_today(
            Some(46),
            60,
            today,
            Some(ymd(2026, 11, 14))
        ));
        // Fresh calendar (exactly threshold — strict `<`) → never fires.
        assert!(!should_alert_today(Some(60), 60, today, None));
        // Empty calendar → maximally stale → fires.
        assert!(should_alert_today(None, 60, today, None));
        // Past the cliff (negative) → fires.
        assert!(should_alert_today(Some(-5), 60, today, None));
    }

    #[test]
    fn test_coverage_end_display_formats() {
        assert_eq!(coverage_end_display(Some(ymd(2026, 12, 31))), "31 Dec 2026");
        assert_eq!(coverage_end_display(None), "none configured");
    }
}
