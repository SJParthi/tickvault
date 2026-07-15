//! Wave 2 Item 5 (G1) — post-close sleep calendar resilience ratchets.
//!
//! PR-C2 trim (2026-07-13): the Dhan live main-feed WS + its pool were
//! DELETED with the lane (operator retirement directive —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"). The three
//! main-feed-machinery ratchets retired WITH the machinery they pinned:
//! - `test_pool_supervisor_respawns_dead_connection_within_5s`
//!   (`WebSocketConnectionPool::supervise_pool` deleted),
//! - `test_token_force_renewal_on_wake_when_stale` (connection.rs wake path
//!   deleted; the order-update twin lives in `ws_sleep_resilience_b.rs`),
//! - `test_no_reconnect_exhaustion_path_remains` (connection.rs deleted).
//!
//! The SURVIVING invariants below pin `TradingCalendar::
//! secs_until_next_market_open` — the shared sleep-until-open primitive the
//! order-update WS (WS-GAP-04) and the Groww dormancy gates still consume:
//!
//! 1. `test_main_feed_post_close_sleeps_until_next_open` — a Friday
//!    16:00 IST snapshot must produce a wake exactly on the next 09:00 IST
//!    trading day (skipping the weekend).
//! 2. `test_70h_sleep_then_connect_succeeds` — the Fri→Mon window equals
//!    65h and stays bounded under 100h.
//! 3. `test_long_weekend_monday_holiday_sleep_wakes_on_tuesday` (W6-2) —
//!    a Monday NSE holiday extends the wake to Tuesday 09:00 IST (89h),
//!    bounded under 120h.

#![allow(clippy::unwrap_used)] // APPROVED: test code
#![allow(clippy::expect_used)] // APPROVED: test code

use chrono::{Datelike, NaiveDate, Weekday};
use tickvault_common::config::{NseHolidayEntry, TradingConfig};
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::trading_calendar::TradingCalendar;

// ---------------------------------------------------------------------------
// Local helpers — duplicated from trading_calendar.rs internal tests so the
// integration crate has no private dependency.
// ---------------------------------------------------------------------------

fn ist_unix(date: NaiveDate, h: u32, m: u32, s: u32) -> i64 {
    let day_secs = i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s);
    let days_from_epoch = i64::from(date.num_days_from_ce()) - 719_163;
    days_from_epoch * 86_400 + day_secs - i64::from(IST_UTC_OFFSET_SECONDS)
}

fn make_calendar() -> TradingCalendar {
    let cfg = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![NseHolidayEntry {
            date: "2026-01-26".to_string(),
            name: "Republic Day".to_string(),
        }],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    };
    TradingCalendar::from_config(&cfg).expect("valid trading config")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_main_feed_post_close_sleeps_until_next_open() {
    // Friday 2026-04-03 16:00 IST → main-feed enters post-close sleep.
    // Expected: wake at Monday 2026-04-06 09:00 IST = 65 hours.
    let cal = make_calendar();
    let friday = NaiveDate::from_ymd_opt(2026, 4, 3).unwrap();
    assert_eq!(friday.weekday(), Weekday::Fri);
    let now = ist_unix(friday, 16, 0, 0);
    let secs = cal
        .secs_until_next_market_open(now)
        .expect("trading day reachable");

    let monday = NaiveDate::from_ymd_opt(2026, 4, 6).unwrap();
    let target = ist_unix(monday, 9, 0, 0);
    assert_eq!(
        i64::try_from(secs).unwrap(),
        target - now,
        "Friday 16:00 IST sleep MUST land exactly on Monday 09:00 IST"
    );
}

#[test]
fn test_70h_sleep_then_connect_succeeds() {
    // Worst-case overnight Fri→Mon sleep window must be bounded
    // strictly under 100 hours and equal exactly the 65-hour gap
    // computed by the trading calendar — i.e., the helper used by
    // connection.rs:wait_with_backoff returns a finite, sane value.
    let cal = make_calendar();
    let friday = NaiveDate::from_ymd_opt(2026, 4, 3).unwrap();
    let now = ist_unix(friday, 16, 0, 0);
    let secs = cal
        .secs_until_next_market_open(now)
        .expect("trading day reachable");
    assert!(
        secs < 100 * 3600,
        "post-close sleep window must be bounded under 100h, got {secs}s"
    );
    assert_eq!(
        secs,
        65 * 3600,
        "Fri 16:00 IST → Mon 09:00 IST must equal 65 hours"
    );
}

// ---------------------------------------------------------------------------
// W6-2 — long-weekend (Monday-holiday) dormant-sleep wake.
//
// `test_70h_sleep_then_connect_succeeds` pins the 65h Fri 16:00 IST → Mon
// 09:00 IST weekend wake. The per-wave-guarantee-matrix / wave-4-shared-preamble
// charter explicitly lists the LONGER holiday-weekend dormant sleep as
// "Outstanding (Wave-6): >65h holiday-weekend dormant sleep ... NOT yet pinned
// by a chaos test", and disaster-recovery.md scenario #15 describes the same
// Wed/Fri-close → Tue-open holiday wake. This test closes that gap: a Friday
// 16:00 IST close with the following Monday declared an NSE holiday must wake
// on TUESDAY 09:00 IST (skipping Sat + Sun + the Monday holiday), proving
// `secs_until_next_market_open` handles a holiday adjacent to the weekend,
// lands exactly on the next real trading day, and stays bounded.
// ---------------------------------------------------------------------------

fn make_calendar_with_holiday(date_ymd: &str, name: &str) -> TradingCalendar {
    let cfg = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![NseHolidayEntry {
            date: date_ymd.to_string(),
            name: name.to_string(),
        }],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    };
    TradingCalendar::from_config(&cfg).expect("valid trading config")
}

#[test]
fn test_long_weekend_monday_holiday_sleep_wakes_on_tuesday() {
    // Monday 2026-04-06 declared an NSE holiday → the Friday 2026-04-03
    // 16:00 IST post-close sleep must wake on Tuesday 2026-04-07 09:00 IST,
    // NOT the Monday (which the plain-weekend test would expect).
    let cal = make_calendar_with_holiday("2026-04-06", "Test Monday Holiday");

    let friday = NaiveDate::from_ymd_opt(2026, 4, 3).unwrap();
    let monday = NaiveDate::from_ymd_opt(2026, 4, 6).unwrap();
    let tuesday = NaiveDate::from_ymd_opt(2026, 4, 7).unwrap();
    assert_eq!(friday.weekday(), Weekday::Fri);
    assert_eq!(monday.weekday(), Weekday::Mon);
    assert_eq!(tuesday.weekday(), Weekday::Tue);

    let now = ist_unix(friday, 16, 0, 0);
    let secs = cal
        .secs_until_next_market_open(now)
        .expect("trading day reachable");

    // Must land EXACTLY on Tuesday 09:00 IST, skipping Sat + Sun + Mon holiday.
    let target = ist_unix(tuesday, 9, 0, 0);
    assert_eq!(
        i64::try_from(secs).unwrap(),
        target - now,
        "Fri 16:00 IST with a Monday NSE holiday MUST wake on Tuesday 09:00 IST \
         (skip Sat + Sun + the Monday holiday), not Monday"
    );

    // Fri 16:00 → Tue 09:00 = 4 days − 7h = 89 hours. It MUST exceed the plain
    // 65h weekend wake and stay bounded under 120h (dormant sleep never runs away).
    assert_eq!(
        secs,
        89 * 3600,
        "Fri 16:00 IST → Tue 09:00 IST across a Monday holiday must equal 89 hours"
    );
    assert!(
        secs > 65 * 3600,
        "long-weekend (holiday) wake must exceed the plain 65h weekend, got {secs}s"
    );
    assert!(
        secs < 120 * 3600,
        "dormant sleep window must stay bounded under 120h, got {secs}s"
    );
}
