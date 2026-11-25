//! B1 review (2026-11-13): the persistence window judges a row with the
//! Muhurat flag of the ROW's own IST date, read from the installed trading
//! calendar, not with the process flag.
//!
//! Before, the writers' window gates (`session_window::verdict`, used by the
//! tick writer, and `arrival_row_is_in_an_open_window`, used by the depth
//! writer) read `muhurat::current()`, the flag of the process doing the
//! writing. A Muhurat evening frame replayed by the next morning's boot (WAL
//! refold or catch-up) was therefore judged out of window: its ticks went to
//! `feed_aux_packets` and its depth rows were dropped for good.
//!
//! Its own binary because the calendar `OnceLock` is process-wide. This
//! process never sets the Muhurat flag (it reads `false`), exactly like a
//! replaying boot on an ordinary day.
//!
//! HONEST SCOPE (B1 review 3, 2026-11-13): the fix matters only for a Muhurat
//! session held in the 18:00 to 19:30 evening window (2024 was 18:00-19:00).
//! The shipped 2026-11-08 session is 14:05 to 15:40, inside the regular
//! 09:00 to 15:40 window, so its rows were in window on any replaying day
//! before this change too, and the fix does nothing for it
//! (`test_shipped_2026_session_rows_are_in_window_on_any_replaying_day`).
//! The 18:30 rows below are a synthetic evening session on that date. A
//! configured session outside BOTH windows cannot ship: the writers' windows
//! are constants and `muhurat_session_calendar.rs` fails the build for such
//! a session, so the per-date flag never has to widen past them.

use std::sync::Arc;

use tickvault_common::config::TradingConfig;
use tickvault_common::session_window::{
    WindowVerdict, arrival_row_is_in_an_open_window, row_day_had_muhurat, row_is_in_an_open_window,
    verdict,
};
use tickvault_common::trading_calendar::TradingCalendar;

/// The shipped `[trading]` section (2026-11-08 is a Muhurat date there).
fn shipped_trading_config() -> TradingConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/base.toml");
    let raw = std::fs::read_to_string(path).expect("config/base.toml");
    let table: toml::Table = raw.parse().expect("base.toml parses");
    table
        .get("trading")
        .expect("[trading]")
        .clone()
        .try_into()
        .expect("[trading] deserializes")
}

/// IST epoch nanoseconds of `date` at `h:m` IST.
fn ist_nanos(y: i32, mo: u32, d: u32, h: u32, m: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)
        .and_then(|day| day.and_hms_opt(h, m, 0))
        .map(|t| t.and_utc().timestamp())
        .expect("valid time")
        .saturating_mul(1_000_000_000)
}

#[test]
fn test_regression_row_day_had_muhurat_keeps_a_muhurat_evening_row_for_a_later_process() {
    let calendar = TradingCalendar::from_config(&shipped_trading_config()).expect("calendar");
    let muhurat_day = chrono::NaiveDate::from_ymd_opt(2026, 11, 8).expect("date");
    assert!(
        calendar.is_muhurat_trading_day(muhurat_day),
        "the shipped calendar must name 2026-11-08 a Muhurat date for this test"
    );
    assert!(tickvault_common::market_hours::set_market_calendar_for_session(Arc::new(calendar)));
    assert!(
        !tickvault_common::muhurat::current(),
        "this process plays a replaying boot on an ordinary day"
    );

    let muhurat_evening = ist_nanos(2026, 11, 8, 18, 30);
    let ordinary_evening = ist_nanos(2026, 11, 9, 18, 30);
    assert!(row_day_had_muhurat(muhurat_evening));
    assert!(!row_day_had_muhurat(ordinary_evening));

    // The tick writer's gate: kept in `ticks` on its own day's flag.
    assert_eq!(
        verdict(muhurat_evening, Some(muhurat_evening)),
        WindowVerdict::InWindow
    );
    assert_eq!(
        verdict(ordinary_evening, Some(ordinary_evening)),
        WindowVerdict::TsOutOfWindow
    );
    assert!(row_is_in_an_open_window(muhurat_evening));
    assert!(!row_is_in_an_open_window(ordinary_evening));
    // The depth writer's arrival-clocked gate.
    assert!(arrival_row_is_in_an_open_window(muhurat_evening));
    assert!(!arrival_row_is_in_an_open_window(ordinary_evening));
    // The regular session is unchanged on either day.
    assert!(row_is_in_an_open_window(ist_nanos(2026, 11, 9, 10, 0)));
}

/// B1 review 3 (2026-11-13): the shipped 2026 Muhurat session (14:05 to
/// 15:40) lies inside the regular capture window, so its rows are in window
/// whatever the Muhurat flag of the replaying process or of the row's date,
/// and the per-date flag changes nothing for them. Pinned so the plan and
/// this binary's docs cannot claim the 2026 session as the case fixed.
#[test]
fn test_shipped_2026_session_rows_are_in_window_on_any_replaying_day() {
    let calendar = TradingCalendar::from_config(&shipped_trading_config()).expect("calendar");
    let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 8).expect("date");
    let (start, end) = calendar
        .muhurat_session_secs(day)
        .expect("the shipped entry has session times");
    let regular = tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST
        ..=tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST;
    assert!(
        regular.contains(&start) && regular.contains(&end),
        "the shipped session lies inside the regular window"
    );
    for (h, m) in [(14, 5), (15, 0), (15, 39)] {
        let row = ist_nanos(2026, 11, 8, h, m);
        for flag in [false, true] {
            assert!(
                tickvault_common::session_window::nanos_in_any_open_window(row, flag),
                "{h}:{m} in window with flag {flag}"
            );
        }
        assert!(row_is_in_an_open_window(row));
        assert_eq!(verdict(row, Some(row)), WindowVerdict::InWindow);
    }
}
