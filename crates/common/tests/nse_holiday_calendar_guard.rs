//! NSE holiday-calendar completeness ratchet (PR-4 M-C, 2026-07-02).
//!
//! A missing/rotted holiday list in `config/base.toml [[trading.nse_holidays]]`
//! makes the system expect market data on a closed day (false Down alarms,
//! wrong boundary tasks). NSE publishes ~14-15 trading holidays per calendar
//! year, so a plausible calendar for the CURRENT year has at least
//! [`MIN_CURRENT_YEAR_HOLIDAYS`] entries. Every January this test goes RED
//! until the new year's calendar is added — the RED build IS the feature.
//!
//! Ratchet contract:
//! 1. `config/base.toml` parses as TOML and carries `trading.nse_holidays`.
//! 2. Every entry has a valid `YYYY-MM-DD` date and a non-empty name.
//! 3. Dates are unique (a duplicate row is a copy-paste rot signal).
//! 4. The CURRENT IST year has >= MIN_CURRENT_YEAR_HOLIDAYS entries.
//!
//! Sibling: `nse_holiday_calendar_currency_guard.rs` (2026-07-01) pins only
//! that the calendar's NEWEST year >= the current IST year — it passes with a
//! single stale-count entry. THIS guard adds count plausibility (a rolled-over
//! calendar with 2 of 14 holidays pasted still fails) + per-entry hygiene.
//! Both are intentional; neither subsumes the other.

use chrono::Datelike;

/// NSE publishes ~14-15 trading holidays/year; 10 is the loose lower bound
/// that catches "the calendar was never rolled over" without flapping on
/// legitimate year-to-year variance.
const MIN_CURRENT_YEAR_HOLIDAYS: usize = 10;

fn base_toml() -> toml::Table {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/base.toml");
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("config/base.toml must exist and be readable at {path}: {e}"));
    raw.parse::<toml::Table>()
        .unwrap_or_else(|e| panic!("config/base.toml must parse as TOML: {e}"))
}

fn holiday_entries(cfg: &toml::Table) -> Vec<(String, String)> {
    cfg.get("trading")
        .and_then(|t| t.get("nse_holidays"))
        .and_then(|h| h.as_array())
        .expect("config/base.toml must carry [[trading.nse_holidays]] entries")
        .iter()
        .map(|entry| {
            let date = entry
                .get("date")
                .and_then(|d| d.as_str())
                .expect("every nse_holidays entry must have a string `date`")
                .to_string();
            let name = entry
                .get("name")
                .and_then(|n| n.as_str())
                .expect("every nse_holidays entry must have a string `name`")
                .to_string();
            (date, name)
        })
        .collect()
}

#[test]
fn test_current_year_holidays_present_and_valid() {
    let cfg = base_toml();
    let entries = holiday_entries(&cfg);
    assert!(
        !entries.is_empty(),
        "trading.nse_holidays must not be empty"
    );

    let mut seen = std::collections::HashSet::new();
    for (date, name) in &entries {
        // Valid, parseable YYYY-MM-DD (rejects "2026-13-40", "26-01-2026", …).
        let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .unwrap_or_else(|e| panic!("holiday date `{date}` ({name}) must be YYYY-MM-DD: {e}"));
        assert_eq!(
            &parsed.format("%Y-%m-%d").to_string(),
            date,
            "holiday date `{date}` must round-trip canonically (no zero-padding drift)"
        );
        assert!(
            !name.trim().is_empty(),
            "holiday name for `{date}` must be non-empty"
        );
        assert!(
            seen.insert(date.clone()),
            "duplicate holiday date `{date}` — copy-paste rot in the calendar"
        );
    }

    // The CURRENT year (IST — Asia/Kolkata drives the trading calendar) must
    // have a plausible NSE count. Every January this fails until the new
    // year's official calendar is added: the RED build is the reminder.
    let current_year = chrono::Utc::now()
        .with_timezone(&chrono_tz::Asia::Kolkata)
        .year();
    let current_year_prefix = format!("{current_year}-");
    let current_year_count = entries
        .iter()
        .filter(|(date, _)| date.starts_with(&current_year_prefix))
        .count();
    assert!(
        current_year_count >= MIN_CURRENT_YEAR_HOLIDAYS,
        "config/base.toml has only {current_year_count} NSE holidays for {current_year} \
         (need >= {MIN_CURRENT_YEAR_HOLIDAYS}). Add the official NSE trading-holiday \
         calendar for {current_year} to [[trading.nse_holidays]]."
    );
}
