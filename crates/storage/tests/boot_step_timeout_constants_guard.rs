//! Sub-PR #2 of 2026-05-27 daily-universe expansion — source-scan ratchet
//! pinning the boot-step deadline + CSV-fetch + universe-size constants
//! declared in `crates/common/src/constants.rs`.
//!
//! Authoritative contract: `.claude/rules/project/
//! daily-universe-scope-expansion-2026-05-27.md` sections §18 (CSV
//! hardening), §19 (boot-step deadlines), §22 (universe-size envelope).
//!
//! Constants pinned here are CONSUMED by:
//! - Sub-PR #3 — CSV downloader (uses `MAX_CSV_BODY_BYTES`,
//!   `INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS`)
//! - Sub-PR #9 — lifecycle reconciler (uses `MAX_DAILY_UNIVERSE_SIZE`,
//!   `MIN_DAILY_UNIVERSE_SIZE`)
//! - Sub-PR #10 — boot orchestrator (uses `BOOT_STEP_*_TIMEOUT_SECS`)
//! - Sub-PR #11 — boot-time-of-day guard (HALT if universe size outside
//!   `[100, 1200]`)
//!
//! This guard fails the build if:
//!  1. Any constant is removed.
//!  2. Any constant's value changes WITHOUT a matching rule-file edit.
//!  3. The rule file stops pinning the values (out-of-sync drift).

#![cfg(test)]

use std::path::PathBuf;

use tickvault_common::constants::{
    BOOT_STEP_AUTH_TIMEOUT_SECS, BOOT_STEP_IP_WHITELIST_TIMEOUT_SECS,
    BOOT_STEP_QUESTDB_DDL_TIMEOUT_SECS, INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS,
    MAX_CSV_BODY_BYTES, MAX_DAILY_UNIVERSE_SIZE, MIN_DAILY_UNIVERSE_SIZE,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn rule_file_body() -> String {
    let path =
        repo_root().join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

// ============================================================================
// Constant-value pinning
// ============================================================================

#[test]
fn boot_step_auth_timeout_pinned_at_60s() {
    assert_eq!(
        BOOT_STEP_AUTH_TIMEOUT_SECS, 60,
        "Step 6 Dhan auth deadline must be 60s per rule file §19 (3 × 20s retries internally)"
    );
}

#[test]
fn boot_step_ip_whitelist_timeout_pinned_at_30s() {
    assert_eq!(
        BOOT_STEP_IP_WHITELIST_TIMEOUT_SECS, 30,
        "Step 6a Dhan IP-whitelist GET deadline must be 30s per rule file §19"
    );
}

#[test]
fn boot_step_questdb_ddl_timeout_pinned_at_60s() {
    assert_eq!(
        BOOT_STEP_QUESTDB_DDL_TIMEOUT_SECS, 60,
        "Step 6b QuestDB DDL deadline must be 60s per rule file §19 (BOOT-01/BOOT-02 runbook)"
    );
}

#[test]
fn instrument_fetch_per_attempt_timeout_pinned_at_60s() {
    assert_eq!(
        INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS, 60,
        "Per-attempt CSV-fetch deadline must be 60s per rule file §18 (wrapped by §4 infinite retry)"
    );
}

#[test]
fn min_daily_universe_size_pinned_at_100() {
    assert_eq!(
        MIN_DAILY_UNIVERSE_SIZE, 100,
        "Minimum universe size must be 100 per rule file §2 + §22"
    );
}

#[test]
fn max_daily_universe_size_pinned_at_1200() {
    assert_eq!(
        MAX_DAILY_UNIVERSE_SIZE, 1200,
        "Maximum universe size must be 1200 per rule file §2 + §31 (NTM expansion, 2026-06-06)"
    );
}

#[test]
fn max_csv_body_bytes_pinned_at_50mb() {
    assert_eq!(
        MAX_CSV_BODY_BYTES,
        50 * 1024 * 1024,
        "CSV body size cap must be 50 MB per rule file §18 (DoS surface bound)"
    );
}

// ============================================================================
// Rule-file cross-reference — values must be mentioned in the contract doc
// ============================================================================

#[test]
fn rule_file_pins_60s_auth_deadline() {
    let body = rule_file_body();
    assert!(
        body.contains("60s total (3 × 20s retries)"),
        "rule file §19 must pin the 60s auth deadline language"
    );
}

#[test]
fn rule_file_pins_30s_ip_whitelist_deadline() {
    let body = rule_file_body();
    // Match the §19 table row: `Step 6a (... `/v2/ip/getIP`) | 30s | ...`
    assert!(
        body.contains("30s"),
        "rule file §19 must pin a 30s deadline for the IP-whitelist step"
    );
}

#[test]
fn rule_file_pins_universe_size_envelope() {
    let body = rule_file_body();
    assert!(
        body.contains("[100, 1200]") || body.contains("MAX_DAILY_UNIVERSE_SIZE = 1200"),
        "rule file §2 / §22 must pin the [100, 1200] universe envelope"
    );
}

#[test]
fn rule_file_pins_csv_body_cap() {
    let body = rule_file_body();
    assert!(
        body.contains("50 MB") || body.contains("MAX_CSV_BODY_BYTES"),
        "rule file §18 must pin the 50 MB CSV body cap"
    );
}

// ============================================================================
// Cross-constant sanity invariants
// ============================================================================

#[test]
fn min_universe_size_less_than_max() {
    assert!(
        MIN_DAILY_UNIVERSE_SIZE < MAX_DAILY_UNIVERSE_SIZE,
        "MIN must be less than MAX — degenerate envelope otherwise"
    );
}

#[test]
fn all_boot_timeouts_are_positive() {
    // Defensive — a zero timeout would mean "no time allowed" which
    // would fail every boot trivially.
    assert!(BOOT_STEP_AUTH_TIMEOUT_SECS > 0);
    assert!(BOOT_STEP_IP_WHITELIST_TIMEOUT_SECS > 0);
    assert!(BOOT_STEP_QUESTDB_DDL_TIMEOUT_SECS > 0);
    assert!(INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS > 0);
}

#[test]
fn auth_timeout_at_least_3x_individual_retry_budget() {
    // Per §19 docstring: "3 × 20s retries" → total ≥ 60s. Defensive
    // ratchet against shrinking the budget without rule-file edit.
    assert!(
        BOOT_STEP_AUTH_TIMEOUT_SECS >= 3 * 20,
        "auth timeout must accommodate 3 × 20s internal retries per §19"
    );
}
