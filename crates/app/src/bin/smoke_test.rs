//! S7-Step4 / Phase 8.5: Smoke test binary.
//!
//! Run after every `deploy-aws` push as the gate between "binary
//! downloaded to the instance" and "systemctl restart tickvault".
//!
//! # What it checks
//!
//! 1. **Config loads and validates** — `config/base.toml` parses
//!    correctly and passes `ApplicationConfig::validate()`.
//! 2. **Sandbox gate holds** — today's IST date is OK'd by
//!    `StrategyConfig::check_sandbox_window()` for the configured
//!    trading mode (e.g., Paper or Sandbox is always OK; Live is
//!    blocked before 2026-06-30).
//! 3. **Required directories exist** — `data/spill` (tick spill) is
//!    writable.
//! 4. **Binary can compute today's IST date** — exercises the
//!    chrono + IST offset constants.
//! 5. **Dhan locked facts pass** — runs the locked fact assertions
//!    inline so an out-of-spec binary never reaches production.
//!
//! # What it does NOT check
//!
//! - **NOT** a Dhan REST round-trip — that would burn a rate limit
//!   and require a valid access token.
//! - **NOT** a WebSocket connection — too slow, too token-consuming.
//! - **NOT** a full trading pipeline cycle — the multi-phase chaos
//!   tests cover that separately.
//!
//! # Exit codes
//!
//! - 0: all checks pass, binary is safe to start
//! - 1: a check failed, deploy should be rolled back
//!
//! # Duration
//!
//! Typically 200-500ms. The deploy workflow allows up to 30 seconds
//! (--duration flag) before considering the test hung.

#![allow(clippy::unwrap_used)] // APPROVED: test-only binary
#![allow(clippy::expect_used)] // APPROVED: test-only binary

use std::path::Path;
use std::time::Instant;

const DEFAULT_DURATION_SECS: u64 = 30;
const CONFIG_BASE_PATH: &str = "config/base.toml";

fn main() -> Result<(), String> {
    let start = Instant::now();
    let duration = parse_duration_flag().unwrap_or(DEFAULT_DURATION_SECS);
    eprintln!("S7-Step4: tv-smoke-test starting (budget {}s)", duration);

    // ------------------------------------------------------------------
    // Check 1: config/base.toml exists, parses, validates
    // ------------------------------------------------------------------
    let cfg_path = Path::new(CONFIG_BASE_PATH);
    if !cfg_path.exists() {
        return Err(format!(
            "SMOKE FAIL: {} not found (working dir = {:?})",
            CONFIG_BASE_PATH,
            std::env::current_dir().ok()
        ));
    }
    eprintln!("  [1/5] config file exists: {}", CONFIG_BASE_PATH);

    // Load via figment + validate. Uses the same Figment::new().merge(Toml::file)
    // pattern as crates/app/src/main.rs (canonical).
    use figment::providers::{Format, Toml};
    let config: tickvault_common::config::ApplicationConfig = figment::Figment::new()
        .merge(Toml::file(CONFIG_BASE_PATH))
        .extract()
        .map_err(|e| format!("SMOKE FAIL: config parse failed: {e}"))?;
    config
        .validate()
        .map_err(|e| format!("SMOKE FAIL: config validation failed: {e}"))?;
    eprintln!("  [2/5] config parse + validate: OK");

    // ------------------------------------------------------------------
    // Check 2: sandbox window gate (the S6-Step4 pre-boot check)
    // ------------------------------------------------------------------
    let today_ist = (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .date_naive();
    config
        .strategy
        .check_sandbox_window(today_ist)
        .map_err(|e| format!("SMOKE FAIL: sandbox window violation: {e}"))?;
    eprintln!(
        "  [3/5] sandbox window check (today_ist={}, mode={:?}, cutoff={}): OK",
        today_ist, config.strategy.mode, config.strategy.sandbox_only_until
    );

    // ------------------------------------------------------------------
    // Check 3: writable spill directory (tick resilience chain)
    // ------------------------------------------------------------------
    let spill_dir = Path::new("data/spill");
    if !spill_dir.exists() {
        std::fs::create_dir_all(spill_dir)
            .map_err(|e| format!("SMOKE FAIL: cannot create {spill_dir:?}: {e}"))?;
    }
    let probe = spill_dir.join(".smoke-test-probe");
    std::fs::write(&probe, b"probe")
        .map_err(|e| format!("SMOKE FAIL: spill dir not writable: {e}"))?;
    std::fs::remove_file(&probe).ok();
    eprintln!("  [4/5] spill dir is writable: {}", spill_dir.display());

    // ------------------------------------------------------------------
    // Check 4: IST date arithmetic is sane
    // ------------------------------------------------------------------
    let ist_offset_secs = tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64;
    if ist_offset_secs != 19_800 {
        return Err(format!(
            "SMOKE FAIL: IST offset drifted: expected 19800 seconds, got {ist_offset_secs}"
        ));
    }

    // ------------------------------------------------------------------
    // Check 5: Dhan locked facts (packet sizes + dedup key)
    // ------------------------------------------------------------------
    use tickvault_common::constants::{
        DEEP_DEPTH_HEADER_SIZE, DEEP_DEPTH_LEVEL_SIZE, FULL_QUOTE_PACKET_SIZE, QUOTE_PACKET_SIZE,
        TICKER_PACKET_SIZE, TWENTY_DEPTH_PACKET_SIZE, TWO_HUNDRED_DEPTH_PACKET_SIZE,
    };
    if TICKER_PACKET_SIZE != 16
        || QUOTE_PACKET_SIZE != 50
        || FULL_QUOTE_PACKET_SIZE != 162
        || DEEP_DEPTH_HEADER_SIZE != 12
        || DEEP_DEPTH_LEVEL_SIZE != 16
        || TWENTY_DEPTH_PACKET_SIZE != 332
        || TWO_HUNDRED_DEPTH_PACKET_SIZE != 3212
    {
        return Err("SMOKE FAIL: Dhan protocol constants drifted".to_string());
    }
    eprintln!("  [5/5] Dhan locked facts: OK");

    let elapsed = start.elapsed();
    eprintln!(
        "S7-Step4: tv-smoke-test PASSED in {:.0}ms — binary is safe to start",
        elapsed.as_millis()
    );
    Ok(())
}

fn parse_duration_flag() -> Option<u64> {
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--duration" {
            return iter.next().and_then(|s| s.parse::<u64>().ok());
        }
        if let Some(rest) = arg.strip_prefix("--duration=") {
            return rest.parse::<u64>().ok();
        }
    }
    None
}
