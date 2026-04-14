//! Sandbox enforcement guard — regression tests.
//!
//! The hard-coded sandbox deadline in `crates/trading/src/oms/engine.rs`
//! is the ONLY thing standing between a future refactor and a real-money
//! order being placed before the system is validated in production.
//!
//! This test file pins the three invariants that must NEVER regress:
//!
//! 1. The constant `SANDBOX_DEADLINE_EPOCH_SECS` exists in `engine.rs` with
//!    the exact numeric value for `2026-07-01T00:00:00 UTC`.
//! 2. The guard actually compares `now_secs < SANDBOX_DEADLINE_EPOCH_SECS`
//!    and returns `OmsError::SandboxEnforcement` on violation.
//! 3. The guard is not silently disabled by a `#[cfg(test)]` rewrite that
//!    masks the real path — the `#[cfg(not(test))]` wrapper is the correct
//!    scoping (tests bypass the check, prod cannot).
//!
//! Source scanning is used instead of calling the function directly because
//! the guard is `#[cfg(not(test))]` scoped — we assert the source code text
//! itself is intact. If the guard is ever removed or the deadline is ever
//! changed, this test fails at compile time or at test time, and a real
//! human has to explicitly acknowledge the change.

use std::fs;

const ENGINE_RS: &str = "src/oms/engine.rs";

/// 2026-07-01T00:00:00 UTC — computed independently so the test is not
/// a tautology against the constant.
///
/// `(2026 - 1970) years` accounting for leap years: the simpler approach
/// is to hard-code the known-good epoch and let a separate assertion verify
/// it matches the `chrono` computation. We do both.
const EXPECTED_DEADLINE_EPOCH_SECS: i64 = 1_782_864_000;

#[test]
fn test_sandbox_deadline_constant_present_in_engine() {
    let source = fs::read_to_string(ENGINE_RS)
        .expect("engine.rs must be readable for sandbox guard regression test");
    assert!(
        source.contains("SANDBOX_DEADLINE_EPOCH_SECS"),
        "engine.rs must define SANDBOX_DEADLINE_EPOCH_SECS — the sandbox guard \
         cannot be removed without explicit review"
    );
    assert!(
        source.contains("1_782_864_000"),
        "SANDBOX_DEADLINE_EPOCH_SECS must retain the literal 1_782_864_000 \
         (2026-07-01T00:00:00 UTC). A future session cannot silently advance \
         the deadline without a code change this test will catch. \
         NOTE: the previous value 1_782_777_600 was a 1-day-too-early bug \
         caught by this test on 2026-04-14."
    );
}

#[test]
fn test_sandbox_guard_returns_sandbox_enforcement_error() {
    let source = fs::read_to_string(ENGINE_RS)
        .expect("engine.rs must be readable for sandbox guard regression test");
    assert!(
        source.contains("now_secs < SANDBOX_DEADLINE_EPOCH_SECS"),
        "The sandbox guard comparison 'now_secs < SANDBOX_DEADLINE_EPOCH_SECS' \
         must be present in engine.rs. If this fails, the guard was removed or \
         inverted."
    );
    assert!(
        source.contains("OmsError::SandboxEnforcement"),
        "The sandbox guard must return OmsError::SandboxEnforcement on violation. \
         Any other error type means the guard was watered down."
    );
}

#[test]
fn test_sandbox_guard_uses_cfg_not_test() {
    let source = fs::read_to_string(ENGINE_RS)
        .expect("engine.rs must be readable for sandbox guard regression test");
    assert!(
        source.contains("#[cfg(not(test))]"),
        "The sandbox guard must be wrapped in #[cfg(not(test))] so tests can \
         bypass it while production code always runs it. Removing this wrapper \
         (or changing it to #[cfg(test)]) is a CRITICAL regression."
    );
}

#[test]
fn test_sandbox_deadline_matches_known_utc_epoch() {
    // Independent verification via chrono — the constant must equal the
    // computed epoch of 2026-07-01T00:00:00 UTC. If chrono and the constant
    // ever disagree, one of them is wrong and this test pins it down.
    use chrono::{TimeZone, Utc};
    let computed = Utc
        .with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
        .single()
        .expect("2026-07-01 is a valid UTC datetime")
        .timestamp();
    assert_eq!(
        computed, EXPECTED_DEADLINE_EPOCH_SECS,
        "Sanity check: the expected constant {EXPECTED_DEADLINE_EPOCH_SECS} \
         must equal chrono's computation of 2026-07-01T00:00:00 UTC ({computed})"
    );
}
