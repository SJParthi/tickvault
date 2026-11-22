//! B1 review 5 (2026-11-13): the candle writer's window gate judges a sealed
//! bar with the Muhurat flag of the BAR's own IST date.
//!
//! B1 review 2 changed `session_window::row_is_in_an_open_window`, which the
//! candle writer (`ShadowCandleWriter::append_row`) calls, from the process
//! Muhurat flag to the row date's, and no test pinned what that does to the
//! candle path. This binary installs the shipped trading calendar in an
//! ordinary-day process (the Muhurat flag reads `false`, like a later boot
//! replaying a Muhurat evening from its log) and checks which bars the
//! writer keeps.
//!
//! Its own binary because the calendar `OnceLock` is process-wide.
//!
//! The 18:30 bars are a synthetic evening session on 2026-11-08 (the shipped
//! 2026 session is 14:05 to 15:40, inside the regular window). Today the
//! fold opens no bucket outside 09:15 to 15:40, so this gate is defence in
//! depth (see the honest-status note at the gate); the test pins the gate's
//! own answer, not that such a bar is folded.

use std::sync::Arc;

use tickvault_common::config::TradingConfig;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_storage::shadow_seal_columns::ShadowSealRow;

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

/// A 1-minute bar opening at 18:30 IST on `d` November 2026.
fn evening_bar(d: u32, security_id: i64) -> ShadowSealRow {
    let ts = chrono::NaiveDate::from_ymd_opt(2026, 11, d)
        .and_then(|day| day.and_hms_opt(18, 30, 0))
        .map(|t| t.and_utc().timestamp())
        .expect("valid time")
        .saturating_mul(1_000_000_000);
    ShadowSealRow {
        table_name: "candles_1m",
        timestamp_ist_nanos: ts,
        security_id,
        segment: "NSE_EQ",
        feed: "dhan",
        open: 100.0,
        high: 105.0,
        low: 99.0,
        close: 101.0,
        volume: 1234,
        oi: 0,
        tick_count: 5,
        close_pct_from_prev_day: 0.0,
        open_pct: 0.0,
        change_pct: 0.0,
        open_gap_pct: 0.0,
        total_buy_qty: 0,
        total_sell_qty: 0,
        open_latency_ns: None,
        close_latency_ns: None,
        window_span_latency_ns: None,
    }
}

/// Regression, B1 review 5: a Muhurat-date evening bar is kept by the candle
/// writer in an ordinary-day process; the same evening on an ordinary date is
/// still refused.
///
/// Bite: make `row_is_in_an_open_window` read `muhurat::current()` instead of
/// the row date's flag (`row_day_had_muhurat`) and the Muhurat bar is refused
/// (the first assert).
#[test]
fn test_regression_candle_writer_keeps_a_muhurat_evening_bar_on_a_later_day() {
    let calendar = TradingCalendar::from_config(&shipped_trading_config()).expect("calendar");
    assert!(
        calendar.is_muhurat_trading_day(chrono::NaiveDate::from_ymd_opt(2026, 11, 8).expect("d")),
        "the shipped calendar must name 2026-11-08 a Muhurat date for this test"
    );
    assert!(tickvault_common::market_hours::set_market_calendar_for_session(Arc::new(calendar)));
    assert!(
        !tickvault_common::muhurat::current(),
        "this process plays a later boot on an ordinary day"
    );

    let mut writer = ShadowCandleWriter::for_test();
    writer
        .append_row(&evening_bar(8, 4242))
        .expect("a refusal or an append is Ok");
    assert_eq!(
        writer.buffer_row_count(),
        1,
        "a Muhurat-date evening bar is kept"
    );
    writer
        .append_row(&evening_bar(9, 4343))
        .expect("a refusal or an append is Ok");
    assert_eq!(
        writer.buffer_row_count(),
        1,
        "an ordinary-date evening bar is still refused"
    );
    let text = std::str::from_utf8(writer.buffer_bytes()).expect("utf8");
    assert!(text.contains("security_id=4242i"), "{text}");
    assert!(!text.contains("security_id=4343i"), "{text}");
}
