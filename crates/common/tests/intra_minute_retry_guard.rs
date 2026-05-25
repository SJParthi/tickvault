//! Ratchets for the 2026-05-25 intra-minute option-chain retry-by-
//! deadline contract (PR #787).
//!
//! Operator demand:
//!   * If any fetch fails inside the `:50..:60` window, INSTANTLY retry.
//!   * Before `:60` boundary, all 3 underlyings' option chain data
//!     MUST be present.
//!   * Per-underlying retry MUST respect Dhan's 1-request-per-3-seconds
//!     per (underlying, expiry) rate limit.
//!
//! These ratchets fail the build if anyone:
//!   * deletes the `burst_orchestrator` module,
//!   * loosens the `:59` hard deadline,
//!   * shortens the 3-second retry spacing,
//!   * raises `MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST` above the
//!     window's safe ceiling,
//!   * removes the intra-minute retry from `run_one_slot_fetch`.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_burst_orchestrator_module_exists() {
    let path = workspace_root().join("crates/core/src/option_chain/burst_orchestrator.rs");
    assert!(
        path.exists(),
        "burst_orchestrator.rs is the canonical contract for the \
         intra-minute retry-by-deadline design"
    );
}

#[test]
fn test_burst_window_constants_match_dhan_rate_limit() {
    let body = read("crates/core/src/option_chain/burst_orchestrator.rs");

    // Operator lock 2026-05-25: window opens at :50, hard deadline :59,
    // 3s spacing per Dhan's rate limit.
    assert!(
        body.contains("pub const BURST_START_SEC_OF_MINUTE: u32 = 50"),
        "BURST_START_SEC_OF_MINUTE MUST be 50"
    );
    assert!(
        body.contains("pub const BURST_DEADLINE_SEC_OF_MINUTE: u32 = 59"),
        "BURST_DEADLINE_SEC_OF_MINUTE MUST be 59 (1s buffer to :00)"
    );
    assert!(
        body.contains("pub const SAME_UNDERLYING_RETRY_SPACING_SECS: u64 = 3"),
        "SAME_UNDERLYING_RETRY_SPACING_SECS MUST be 3 — matches Dhan's \
         per-underlying rate limit"
    );
    assert!(
        body.contains("pub const MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST: u32 = 3"),
        "MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST MUST be 3 — derived from \
         (10s window / 3s spacing)"
    );
}

#[test]
fn test_scheduler_run_one_slot_fetch_uses_intra_minute_retry() {
    let body = read("crates/core/src/option_chain/snapshot_scheduler.rs");
    // The retry loop pulls from the burst_orchestrator's constants +
    // helpers — pinned so a refactor can't silently revert to
    // "fail on first error, no retry".
    assert!(
        body.contains("burst_orchestrator::MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST"),
        "run_one_slot_fetch MUST cap retries at MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST"
    );
    assert!(
        body.contains("burst_orchestrator::next_retry_sleep_secs"),
        "run_one_slot_fetch MUST consult next_retry_sleep_secs for \
         retry-vs-skip decisions"
    );
    assert!(
        body.contains("burst_orchestrator::BURST_DEADLINE_SEC_OF_MINUTE"),
        "run_one_slot_fetch MUST respect the :59 hard deadline"
    );
    assert!(
        body.contains("RECOVERED via intra-minute retry"),
        "run_one_slot_fetch MUST emit a positive log line when a retry \
         succeeds — operator visibility per audit-findings Rule 11"
    );
}

#[test]
fn test_dhan_rate_limit_3s_spacing_is_pinned_in_client() {
    // The client's internal rate limit MUST be at least 3 seconds. If
    // someone shortens it below 3s, this ratchet fires and the build
    // breaks. Dhan's docs are explicit: "Rate limit ... is set to one
    // unique request every 3 seconds" — anything less is a guaranteed
    // DH-904 rate-limit violation.
    let body = read("crates/common/src/constants.rs");
    assert!(
        body.contains("OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS: u64 = 3")
            || body.contains("OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS = 3"),
        "OPTION_CHAIN_MIN_REQUEST_INTERVAL_SECS MUST be 3 seconds — \
         Dhan's per-(instrument, expiry) rate limit"
    );
}

#[test]
fn test_burst_max_attempts_does_not_exceed_window_budget() {
    // Source-scan version (tickvault-common has no dep on
    // tickvault-core): assert the numeric relation by reading the
    // constant declarations from the burst_orchestrator source file.
    let body = read("crates/core/src/option_chain/burst_orchestrator.rs");

    // Window math: window = (DEADLINE - START) = 59 - 50 = 9s.
    // MAX_ATTEMPTS = 3. Per-underlying spacing = 3s.
    // 3 attempts × 3s spacing = 9s, fits exactly in the window.
    // Any change here MUST keep the relation: MAX_ATTEMPTS * SPACING <= WINDOW.
    assert!(
        body.contains("BURST_START_SEC_OF_MINUTE: u32 = 50")
            && body.contains("BURST_DEADLINE_SEC_OF_MINUTE: u32 = 59"),
        "window constants must remain 50/59 — refactor would break the \
         retry-budget calculation"
    );
    assert!(
        body.contains("SAME_UNDERLYING_RETRY_SPACING_SECS: u64 = 3")
            && body.contains("MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST: u32 = 3"),
        "spacing × max_attempts MUST equal exactly 9 (window width) — \
         changing either side without the other risks deadline overshoot"
    );
}

#[test]
fn test_burst_orchestrator_helpers_have_been_pure_function_unit_tested() {
    // Source-scan ratchet: the orchestrator module MUST keep its 4
    // public test functions covering the math primitives. Future
    // refactors that delete these tests would let a retry-spacing
    // bug ship undetected.
    let body = read("crates/core/src/option_chain/burst_orchestrator.rs");
    for fn_name in [
        "test_compute_secs_until_next_burst_before_50",
        "test_compute_secs_until_next_burst_exactly_at_50",
        "test_compute_secs_until_next_burst_after_50",
        "test_next_retry_sleep_after_first_failure_returns_3s",
        "test_next_retry_sleep_after_third_failure_returns_none",
        "test_burst_window_is_10_seconds_wide",
        "test_max_attempts_is_3_per_underlying",
    ] {
        assert!(
            body.contains(fn_name),
            "burst_orchestrator MUST keep test `{fn_name}` — pure-function \
             primitives are the only thing protecting the design from drift"
        );
    }
}
