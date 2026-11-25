//! Build-failing ratchet (2026-11-07): the Lambdas' compiled NSE holiday list
//! (`nse_calendar::NSE_HOLIDAYS`) must name exactly the
//! `[[trading.nse_holidays]]` dates in `config/base.toml`, the app's one
//! calendar source. A holiday added to the config and not here would let the
//! deploy watchdog redeploy, and the boot-heartbeat gate page, on that
//! holiday.

#![cfg(test)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use tickvault_aws_lambdas::nse_calendar::NSE_HOLIDAYS;

fn base_toml() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/base.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The `date = "..."` value of every `[[trading.nse_holidays]]` table.
fn config_holidays(toml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_holiday = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_holiday = t == "[[trading.nse_holidays]]";
            continue;
        }
        if in_holiday && let Some(rest) = t.strip_prefix("date = \"") {
            out.push(rest.trim_end_matches('"').to_string());
        }
    }
    out
}

#[test]
fn compiled_holidays_equal_config_base_toml() {
    let config = config_holidays(&base_toml());
    assert!(
        config.len() >= 10,
        "config/base.toml must still list the NSE holidays (found {})",
        config.len()
    );
    let compiled: Vec<String> = NSE_HOLIDAYS.iter().map(|s| (*s).to_string()).collect();
    let config_set: BTreeSet<_> = config.iter().cloned().collect();
    let compiled_set: BTreeSet<_> = compiled.iter().cloned().collect();
    assert_eq!(
        compiled_set, config_set,
        "nse_calendar::NSE_HOLIDAYS must equal config/base.toml [[trading.nse_holidays]]: \
         update both in the same change"
    );
    assert_eq!(compiled.len(), compiled_set.len(), "no duplicate holiday");
}

#[test]
fn guard_self_test_parser_reads_only_holiday_tables() {
    let sample = "[[trading.nse_holidays]]\ndate = \"2026-01-26\"\nname = \"x\"\n\n\
                  [[trading.muhurat_trading_dates]]\ndate = \"2026-11-08\"\n\n\
                  [[trading.nse_holidays]]\ndate = \"2026-12-25\"\n";
    assert_eq!(
        config_holidays(sample),
        vec!["2026-01-26".to_string(), "2026-12-25".to_string()],
        "a Muhurat or mock date must never be read as a holiday"
    );
}
