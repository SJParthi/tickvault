//! Ratchet: the NSE holiday calendar in `config/base.toml` MUST cover the
//! current IST year, or the unattended AWS box silently runs full billable
//! sessions on every weekday NSE holiday it doesn't know about.
//!
//! Why (HIGH finding, 2026-07-01 adversarial automation hunt): `is_trading_day`
//! (`crates/common/src/trading_calendar.rs`) treats ANY weekday not explicitly
//! listed in `nse_holidays` as a trading day — there is no year bound. The
//! holiday-gate self-stop (`deploy/aws/holiday-gate.sh`) relies on that calendar.
//! So if the list only contains (say) 2026 dates and the box runs into 2027,
//! every 2027 weekday holiday (Republic Day, Holi, Diwali, …) is treated as a
//! trading day: the box starts, both feeds connect, and it burns a full ~8h of
//! billing on a market-closed day (and reports a false "market open"). The
//! failure direction is cost/false-open, not missed-trading-day — but it is a
//! real unattended-automation gap with nothing forcing the calendar forward.
//!
//! This guard makes an EXPIRED calendar a BUILD FAILURE the moment the IST year
//! rolls past the newest listed holiday, so a human is forced to paste the next
//! NSE circular BEFORE the box runs stale — instead of discovering it from an
//! AWS bill. It is intentionally time-aware (reads the real current IST year);
//! that is the whole point of a "keep the calendar current" ratchet.
//!
//! Passes today (config has 2026 dates, current IST year 2026). Goes red on
//! 2027-01-01 unless 2027's holidays are added — the intended forcing function.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Datelike, Duration, Utc};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

/// Current IST calendar year (UTC + 05:30). Test-time wall-clock is the intended
/// input — this ratchet's job is to compare the calendar against "now".
fn current_ist_year() -> i32 {
    (Utc::now() + Duration::hours(5) + Duration::minutes(30)).year()
}

/// Extract the max 4-digit year across every `[[trading.nse_holidays]]` block's
/// `date = "YYYY-MM-DD"` in config/base.toml. Scoped to the nse_holidays
/// sections so muhurat dates / other `date =` keys never leak in.
fn max_nse_holiday_year(toml: &str) -> Option<i32> {
    let mut max_year: Option<i32> = None;
    // Each holiday is its own `[[trading.nse_holidays]]` table; the date line
    // is the first `date = "..."` after that header.
    for block in toml.split("[[trading.nse_holidays]]").skip(1) {
        // Stop at the next table header so we only read THIS holiday's date.
        let scope = block.split("[[").next().unwrap_or(block);
        if let Some(date_line) = scope.lines().find(|l| l.trim_start().starts_with("date")) {
            if let Some(q0) = date_line.find('"') {
                let after = &date_line[q0 + 1..];
                if let Some(year) = after.get(0..4).and_then(|y| y.parse::<i32>().ok()) {
                    max_year = Some(max_year.map_or(year, |m| m.max(year)));
                }
            }
        }
    }
    max_year
}

#[test]
fn test_nse_holiday_calendar_covers_current_ist_year() {
    let toml = {
        let p = workspace_root().join("config/base.toml");
        fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
    };
    let max_year = max_nse_holiday_year(&toml)
        .expect("config/base.toml must have at least one [[trading.nse_holidays]] date");
    let now_year = current_ist_year();
    assert!(
        max_year >= now_year,
        "NSE holiday calendar is STALE: newest listed holiday is year {max_year}, but the \
         current IST year is {now_year}. is_trading_day() treats any unlisted weekday as a \
         TRADING day, so the unattended AWS box will START + connect both feeds + burn ~8h \
         of billing on every weekday NSE holiday of {now_year} (and report a false 'market \
         open'). FIX: paste year {now_year}'s NSE holiday circular into config/base.toml \
         [[trading.nse_holidays]]."
    );
}

#[test]
fn test_max_nse_holiday_year_parser_is_scoped_and_correct() {
    // The parser reads ONLY nse_holidays blocks, takes the max, and ignores
    // muhurat dates / unrelated `date =` keys.
    let sample = r#"
sandbox_only_until = "2099-01-01"
[[trading.nse_holidays]]
date = "2026-01-26"
name = "Republic Day"
[[trading.nse_holidays]]
date = "2026-11-10"
name = "Diwali"
[[trading.muhurat_trading_dates]]
date = "2030-11-08"
name = "Muhurat"
"#;
    assert_eq!(
        max_nse_holiday_year(sample),
        Some(2026),
        "parser must take the max nse_holiday year (2026) and ignore the 2099 \
         sandbox date + the 2030 muhurat date"
    );
    assert_eq!(max_nse_holiday_year("no holidays here"), None);
}
