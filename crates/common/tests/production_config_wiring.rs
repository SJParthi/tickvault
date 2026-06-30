//! Single-prod-env lockdown test for config/production.toml (operator 2026-06-30).
//!
//! Replaces the retired `staging_config_wiring.rs` (config/staging.toml was
//! deleted when dev/staging were collapsed into the single `prod` env).
//!
//! Ensures the production profile exists, parses cleanly, and — the HARD safety
//! constraint the operator has insisted on dozens of times — locks
//! `dry_run = true` (NO real orders) with a far-future `sandbox_only_until`
//! belt-and-suspenders so it can never accidentally promote to live trading.
//! This is still the no-real-orders data-pull phase.

use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

#[test]
fn test_production_config_exists_and_parses() {
    let path = workspace_root().join("config").join("production.toml");
    assert!(
        path.exists(),
        "config/production.toml missing — the single real env config is gone"
    );
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("production.toml not readable: {e}")); // APPROVED: test
    assert!(
        content.len() > 100,
        "production.toml suspiciously short ({} bytes)",
        content.len()
    );
}

#[test]
fn test_production_locks_dry_run_no_real_orders() {
    let content = std::fs::read_to_string(workspace_root().join("config").join("production.toml"))
        .expect("production.toml must be readable"); // APPROVED: test

    // HARD SAFETY: dry_run MUST be true — this is the no-real-orders data-pull
    // phase. A build-failing assertion so dry_run can never silently flip back
    // to false (which would arm real order placement).
    assert!(
        content.contains("dry_run = true"),
        "PROD LOCKDOWN: production.toml MUST set dry_run = true (NO real orders)"
    );
    assert!(
        !content.contains("dry_run = false"),
        "PROD LOCKDOWN: production.toml MUST NOT set dry_run = false — that arms REAL orders"
    );
    // mode MUST never be live in this phase.
    assert!(
        !content.contains("mode = \"live\""),
        "PROD LOCKDOWN: mode = live is forbidden in production.toml (data-pull phase)"
    );
    // Belt-and-suspenders: a far-future sandbox_only_until makes accidental
    // live promotion mechanically impossible at boot even if mode is misedited.
    assert!(
        content.contains("2099-12-31"),
        "PROD LOCKDOWN: sandbox_only_until must be 2099-12-31 to prevent accidental \
         Live promotion (belt-and-suspenders defense-in-depth)"
    );
}

#[test]
fn test_production_runs_both_feeds() {
    let content = std::fs::read_to_string(workspace_root().join("config").join("production.toml"))
        .expect("production.toml must be readable"); // APPROVED: test

    // Both market-data feeds run under the single prod env (operator 2026-06-30).
    // Neither places orders — dry_run=true gates all order placement.
    assert!(
        content.contains("dhan_enabled = true"),
        "production.toml must enable the Dhan feed (feed #1)"
    );
    assert!(
        content.contains("groww_enabled = true"),
        "production.toml must enable the Groww feed (feed #2)"
    );
}

#[test]
fn test_staging_and_dev_configs_are_retired() {
    // Single-prod-env consolidation (operator 2026-06-30): base.toml +
    // production.toml are the ONLY env configs. config/staging.toml and
    // config/dev.toml must not exist.
    assert!(
        !workspace_root()
            .join("config")
            .join("staging.toml")
            .exists(),
        "config/staging.toml must be retired (single prod env)"
    );
    assert!(
        !workspace_root().join("config").join("dev.toml").exists(),
        "there must be no config/dev.toml (single prod env)"
    );
}
