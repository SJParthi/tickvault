//! Coverage-horizon MECHANISM guard (W2 PR#5, 2026-07-10 — audit
//! follow-up row 15).
//!
//! Pins that the REAL `config/base.toml` holiday calendar produces a
//! well-defined coverage horizon through the runtime staleness guard's
//! own primitives — i.e. the mechanism the operator's Telegram page
//! depends on can never silently stop computing against the shipped
//! config.
//!
//! Deliberately NOT pinned: "2027 must be present". NSE has not published
//! the 2027 circular; hardcoding future dates would either red the build
//! permanently today or force fabricated holiday dates (a data-integrity
//! violation). The FORWARD-looking pressure is the runtime watchdog
//! (`crates/app/src/calendar_staleness.rs`, pages < 60 days before the
//! cliff); the POST-cliff build pressure already exists in the sibling
//! guards (`nse_holiday_calendar_guard.rs` +
//! `nse_holiday_calendar_currency_guard.rs`, red on Jan 1).

use chrono::Datelike;
use tickvault_common::config::TradingConfig;
use tickvault_common::trading_calendar::{TradingCalendar, is_calendar_coverage_stale};

/// Loads the REAL shipped config's `[trading]` section.
fn shipped_trading_config() -> TradingConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/base.toml");
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("config/base.toml must be readable at {path}: {e}"));
    let table: toml::Table = raw
        .parse()
        .unwrap_or_else(|e| panic!("config/base.toml must parse as TOML: {e}"));
    let trading = table
        .get("trading")
        .expect("config/base.toml must carry a [trading] section");
    trading
        .clone()
        .try_into()
        .unwrap_or_else(|e| panic!("[trading] must deserialize into TradingConfig: {e}"))
}

/// The shipped calendar's coverage end must PARSE and land on Dec 31 of a
/// plausible year (>= 2026 — the year of the oldest circular the repo has
/// ever shipped; a smaller year means the list was gutted/rotted). This is
/// the mechanism pin: `coverage_end_date()` is the exact value the runtime
/// watchdog alerts from.
#[test]
fn test_base_toml_coverage_end_parses_to_dec31() {
    let calendar = TradingCalendar::from_config(&shipped_trading_config())
        .expect("shipped nse_holidays must build a TradingCalendar");
    let end = calendar
        .coverage_end_date()
        .expect("shipped calendar must have a non-empty nse_holidays list");
    assert_eq!(end.month(), 12, "coverage end must be Dec 31: got {end}");
    assert_eq!(end.day(), 31, "coverage end must be Dec 31: got {end}");
    assert!(
        end.year() >= 2026,
        "coverage end year rotted below 2026: got {end}"
    );
}

/// The staleness classifier must give a definite verdict against the REAL
/// shipped calendar (never a panic / never an unset state) — the exact
/// call chain the runtime watchdog executes each check. The verdict's
/// VALUE is time-dependent by design (stale near/after the cliff is the
/// feature, not flake), so only totality + consistency with the raw day
/// count is asserted — not the boolean itself.
#[test]
fn test_shipped_calendar_staleness_verdict_is_total_and_consistent() {
    let calendar = TradingCalendar::from_config(&shipped_trading_config())
        .expect("shipped nse_holidays must build a TradingCalendar");
    let today = chrono::Utc::now()
        .with_timezone(&tickvault_common::trading_calendar::ist_offset())
        .date_naive();
    let remaining = calendar.coverage_days_remaining(today);
    let days = remaining.expect("non-empty shipped calendar must yield Some(days)");
    let stale = is_calendar_coverage_stale(
        remaining,
        tickvault_common::constants::CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS,
    );
    assert_eq!(
        stale,
        days < tickvault_common::constants::CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS,
        "classifier must agree with the raw day count (days={days})"
    );
}
