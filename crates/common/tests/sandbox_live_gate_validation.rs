//! D1 sandbox-gate validation — date-robust coverage.
//!
//! The inline `test_sandbox_guard_blocks_live_before_july` only calls
//! `validate()` with `mode = Live` when today < `LIVE_TRADING_EARLIEST_DATE`
//! (2026-07-01) — so from 2026-07-01 onward the entire live-mode guard block
//! in `ApplicationConfig::validate()` silently fell OUT of coverage (this is
//! exactly the `common` coverage regression the gate flagged on main).
//!
//! This test runs the live-mode `validate()` path UNCONDITIONALLY, against the
//! real production `config/base.toml`, and asserts the correct branch for
//! whichever side of the earliest-date boundary "today" is on — so the guard
//! stays covered forever, in both eras.

use figment::Figment;
use figment::providers::{Format, Toml};
use tickvault_common::config::{ApplicationConfig, TradingMode};

/// The production config, exactly as boot loads it (figment + TOML).
fn load_base_config() -> ApplicationConfig {
    Figment::new()
        .merge(Toml::file("../../config/base.toml"))
        .extract()
        .expect("config/base.toml must deserialize into ApplicationConfig")
}

fn today_ist() -> chrono::NaiveDate {
    (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive()
}

fn live_trading_earliest() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(
        tickvault_common::constants::LIVE_TRADING_EARLIEST_YEAR,
        tickvault_common::constants::LIVE_TRADING_EARLIEST_MONTH,
        tickvault_common::constants::LIVE_TRADING_EARLIEST_DAY,
    )
    .expect("LIVE_TRADING_EARLIEST_* constants form a real date")
}

#[test]
fn production_base_toml_validates_clean() {
    // The checked-in prod config must always pass its own validation
    // (URL schemes, holiday calendar, historical bounds, sandbox gate).
    let config = load_base_config();
    config
        .validate()
        .expect("the production config/base.toml must validate");
    // The locked operator default: never live out of the box.
    assert!(
        !config.strategy.mode.is_live(),
        "base.toml must never ship with mode = live"
    );
}

#[test]
fn live_mode_validate_runs_the_d1_guard_in_both_eras() {
    // ALWAYS drive validate() through the live-mode D1 guard — unlike the
    // date-gated inline test, this exercises the guard's date computation on
    // every run, and asserts the correct outcome for the current era.
    let mut config = load_base_config();
    config.strategy.mode = TradingMode::Live;

    let result = config.validate();
    if today_ist() < live_trading_earliest() {
        // Pre-earliest era: live mode must be REFUSED with the exact marker.
        let err = result.expect_err("live mode before the earliest date must be refused");
        assert!(
            err.to_string().contains("SANDBOX GUARD"),
            "refusal must carry the SANDBOX GUARD marker: {err}"
        );
    } else {
        // Post-earliest era: the D1 date gate passes and validation proceeds
        // through the URL checks — the guard block still EXECUTES (coverage),
        // it just no longer bails.
        result.expect("live mode on/after the earliest date must pass validation");
    }
}
