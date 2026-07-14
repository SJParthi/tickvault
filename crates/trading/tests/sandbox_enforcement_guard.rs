//! Sandbox enforcement guard — regression tests.
//!
//! The sandbox deadline consumed by `crates/trading/src/oms/engine.rs` is the
//! ONLY mode-independent gate standing between a future refactor and a
//! real-money order being placed before the operator explicitly re-arms for
//! live trading.
//!
//! 2026-07-14 re-arm: the previous fn-local 2026-07-01 deadline
//! (`1_782_864_000`) EXPIRED silently on 2026-07-01, leaving the gate a
//! no-op. The constant now lives in
//! `tickvault_common::constants::SANDBOX_DEADLINE_EPOCH_SECS` (single source
//! of truth, 2099-12-31T00:00:00Z sentinel = `4_102_358_400`, matching
//! production.toml's `sandbox_only_until`). This test file pins the
//! invariants that must NEVER regress:
//!
//! 1. `engine.rs` imports + compares the SHARED constant (by NAME) inside a
//!    `#[cfg(not(test))]` block and returns `OmsError::SandboxEnforcement`.
//! 2. `constants.rs` retains the exact 2099-12-31T00:00:00Z literal,
//!    cross-checked against a chrono computation (imported programmatically —
//!    never a tautology against a local copy).
//! 3. The epoch sentinel stays CALENDAR-ALIGNED with the config-level
//!    `LIVE_TRADING_EARLIEST_*` gate (same 2099-12-31 day).
//!
//! Source scanning is used for the engine.rs shape because the guard is
//! `#[cfg(not(test))]` scoped — we assert the source code text itself is
//! intact. If the guard is ever removed or the deadline is ever changed, this
//! test fails and a real human has to explicitly acknowledge the change with
//! a fresh dated operator quote.

use std::fs;

use tickvault_common::constants::{
    LIVE_TRADING_EARLIEST_DAY, LIVE_TRADING_EARLIEST_MONTH, LIVE_TRADING_EARLIEST_YEAR,
    SANDBOX_DEADLINE_EPOCH_SECS,
};

const ENGINE_RS: &str = "src/oms/engine.rs";

/// Path to the shared constants file (relative to the trading crate root).
const CONSTANTS_RS: &str = "../common/src/constants.rs";

#[test]
fn test_sandbox_deadline_constant_referenced_in_engine() {
    let source = fs::read_to_string(ENGINE_RS)
        .expect("engine.rs must be readable for sandbox guard regression test");
    assert!(
        source.contains("tickvault_common::constants::SANDBOX_DEADLINE_EPOCH_SECS"),
        "engine.rs must import the SHARED tickvault_common::constants::\
         SANDBOX_DEADLINE_EPOCH_SECS — the fn-local copy expired silently on \
         2026-07-01 (the 2026-07-14 re-arm moved it to a single source of \
         truth); the sandbox guard cannot be removed or re-localized without \
         explicit review."
    );
    assert!(
        !source.contains("const SANDBOX_DEADLINE_EPOCH_SECS"),
        "engine.rs must NOT re-declare a fn-local SANDBOX_DEADLINE_EPOCH_SECS \
         — a local copy is exactly the silent-expiry drift class the \
         2026-07-14 re-arm eliminated."
    );
}

#[test]
fn test_sandbox_deadline_literal_present_in_constants() {
    let source = fs::read_to_string(CONSTANTS_RS)
        .expect("common constants.rs must be readable for sandbox guard regression test");
    assert!(
        source.contains("4_102_358_400"),
        "constants.rs must retain the literal 4_102_358_400 \
         (2099-12-31T00:00:00Z sentinel). A future session cannot silently \
         change the deadline without a code change this test will catch — a \
         fresh dated operator quote is required to go live. \
         NOTE (history): 1_782_777_600 was a 1-day-too-early bug caught \
         2026-04-14; 1_782_864_000 (2026-07-01) then EXPIRED silently — the \
         sentinel + this guard close that class."
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
    assert!(
        source.contains("blocked pending explicit"),
        "The sandbox guard's error! message must state that live orders are \
         blocked pending explicit re-arm (sentinel wording) — a stale \
         fixed-date message misleads the operator about when the gate lifts."
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
    // Independent verification via chrono — the IMPORTED constant must equal
    // the computed epoch of 2099-12-31T00:00:00Z. Importing the constant
    // programmatically is stronger than a literal pin: chrono and the shipped
    // value can never disagree silently.
    use chrono::{TimeZone, Utc};
    let computed = Utc
        .with_ymd_and_hms(2099, 12, 31, 0, 0, 0)
        .single()
        .expect("2099-12-31 is a valid UTC datetime")
        .timestamp();
    assert_eq!(
        computed, SANDBOX_DEADLINE_EPOCH_SECS,
        "The shipped SANDBOX_DEADLINE_EPOCH_SECS ({SANDBOX_DEADLINE_EPOCH_SECS}) \
         must equal chrono's computation of 2099-12-31T00:00:00Z ({computed})"
    );
}

#[test]
fn test_sandbox_deadline_aligned_with_live_trading_earliest_date() {
    // Tri-gate alignment: the OMS epoch sentinel's UTC calendar date must be
    // the SAME 2099-12-31 day as the config-level LIVE_TRADING_EARLIEST_*
    // gate (the epoch is midnight UTC; the config gate compares IST calendar
    // dates — the 5h30m nuance stays inside the same calendar day, documented
    // on the constant). If either gate is ever re-armed alone, this trips.
    use chrono::{DateTime, NaiveDate};
    let epoch_utc_date = DateTime::from_timestamp(SANDBOX_DEADLINE_EPOCH_SECS, 0)
        .expect("sentinel epoch must be representable")
        .date_naive();
    let earliest = NaiveDate::from_ymd_opt(
        LIVE_TRADING_EARLIEST_YEAR,
        LIVE_TRADING_EARLIEST_MONTH,
        LIVE_TRADING_EARLIEST_DAY,
    )
    .expect("LIVE_TRADING_EARLIEST_* must form a valid date");
    assert_eq!(
        epoch_utc_date, earliest,
        "SANDBOX_DEADLINE_EPOCH_SECS (UTC date {epoch_utc_date}) must stay \
         calendar-aligned with LIVE_TRADING_EARLIEST_* ({earliest}) — the two \
         date gates re-arm together, never one alone."
    );
}
