//! S7-Step4 / Phase 8.5: Smoke test binary.
//!
//! Run after every `deploy-aws` push as the gate between "binary
//! downloaded to the instance" and "systemctl restart tickvault".
//!
//! # What it checks
//!
//! 1. **Effective config loads and validates** — the same base → environment
//!    → local layers as the application pass `ApplicationConfig::validate()`.
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
    // Check 1: the application's effective config exists, parses, validates
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

    let config_env = tickvault_app::boot_helpers::resolve_config_env();
    let config = load_effective_config(Path::new("."), &config_env)?;
    config
        .validate()
        .map_err(|_| "SMOKE FAIL: effective config validation failed".to_owned())?;
    eprintln!("  [2/5] effective base/environment/local config parse + validate: OK");

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

fn load_effective_config(
    root: &Path,
    environment: &str,
) -> Result<tickvault_common::config::ApplicationConfig, String> {
    use figment::providers::{Format, Toml};
    use tickvault_app::boot_helpers::{CONFIG_LOCAL_PATH, config_env_path};

    let mut figment = figment::Figment::new().merge(Toml::file(root.join(CONFIG_BASE_PATH)));
    if let Some(path) = config_env_path(environment) {
        figment = figment.merge(Toml::file(root.join(path)));
    }
    // Match main.rs: merge all layers before deserializing. Do not print
    // Figment's error payload, which can contain a value from local config.
    figment
        .merge(Toml::file(root.join(CONFIG_LOCAL_PATH)))
        .extract()
        .map_err(|_| "SMOKE FAIL: effective base/environment/local config parse failed".to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct ConfigFixture(std::path::PathBuf);

    impl ConfigFixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "tickvault-smoke-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(root.join("config")).unwrap();
            std::fs::write(
                root.join(CONFIG_BASE_PATH),
                include_str!("../../../../config/base.toml"),
            )
            .unwrap();
            Self(root)
        }
    }

    impl Drop for ConfigFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn smoke_rejects_retired_timeframes_from_the_production_override() {
        let fixture = ConfigFixture::new();
        std::fs::write(
            fixture.0.join("config/production.toml"),
            "[engine.timeframes]\nlist = [\"1d\"]\n",
        )
        .unwrap();
        assert!(load_effective_config(&fixture.0, "prod").is_err());
        assert!(load_effective_config(&fixture.0, "production").is_err());
        // dev intentionally has no environment override, as in main.rs.
        assert!(load_effective_config(&fixture.0, "dev").is_ok());
    }

    #[test]
    fn smoke_applies_local_last_before_deserializing_and_redacts_errors() {
        let fixture = ConfigFixture::new();
        std::fs::write(
            fixture.0.join("config/production.toml"),
            "[engine.timeframes]\nlist = [\"1d\"]\n",
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("config/local.toml"),
            "[engine.timeframes]\nlist = [\"60m\", \"30m\", \"15m\", \"10m\", \"5m\", \"3m\", \"1m\", \"5s\", \"3s\", \"1s\"]\n",
        )
        .unwrap();
        let config = load_effective_config(&fixture.0, "prod").unwrap();
        assert_eq!(
            config.engine.timeframes.list,
            tickvault_common::config::TimeframesConfig::default_list()
        );
        std::fs::write(
            fixture.0.join("config/local.toml"),
            "[engine.timeframes]\nlist = [\"fixture-secret-must-never-be-logged\"]\n",
        )
        .unwrap();
        let error = load_effective_config(&fixture.0, "prod").err().unwrap();
        assert!(!error.contains("fixture-secret"));
    }
}
