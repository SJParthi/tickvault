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

/// The gate must be INSIDE `place_order`, and above the network call.
///
/// # Why the four `contains()` checks above are not enough
///
/// Every assertion in this file before this one asks whether a string exists
/// ANYWHERE in `engine.rs`. That is a question about the file, not about the
/// code path a live order takes. An adversarial sweep on 2026-09-05 named the
/// bypass precisely: lift the `#[cfg(not(test))]` block out of `place_order`
/// into a helper that only the paper path calls. Every literal those four
/// tests look for is still in the file — the comparison, the error variant,
/// the message, the `cfg` — so all four stay green while a live order walks
/// straight past the gate.
///
/// This is the same class as `capture_at_receipt_order_guard`, which on the
/// same day was found reporting "the frame is refused before the ring" about
/// code that forwards it: presence and position cannot see structure. It
/// matters more here, because the thing on the other side of this gate is
/// real money.
///
/// So this test asks the two structural questions instead:
///
///   1. is the gate inside `place_order`'s own body, and
///   2. does it sit above the first thing that could reach the broker?
///
/// The sibling `dhan_exit_order_lockout_guard` already pins its exit methods
/// this way with a `// LIVE-EXIT-ARM` sentinel. The entry path had no
/// equivalent until now.
#[test]
fn the_sandbox_gate_is_inside_place_order_and_above_the_network_call() {
    let source = fs::read_to_string(ENGINE_RS).expect("engine.rs must be readable");

    // Bound the window to `place_order`'s own body. A gate in a neighbouring
    // function is exactly the bypass this test exists to reject.
    let fn_start = source
        .find("pub async fn place_order(&mut self, request: PlaceOrderRequest)")
        .expect(
            "place_order must exist with this signature — if it was renamed, \
             update this guard rather than deleting it",
        );
    let body = &source[fn_start..];
    let fn_end = body.find("\n    }\n").map_or(body.len(), |i| i + 6);
    let body = &body[..fn_end];

    assert!(
        body.len() > 500,
        "extracted place_order body is implausibly short ({} bytes) — the \
         scanner is broken and every assertion below would pass vacuously",
        body.len()
    );

    let gate = body
        .find("if now_secs < SANDBOX_DEADLINE_EPOCH_SECS")
        .expect(
            "THE SANDBOX GATE IS NO LONGER INSIDE `place_order`.\n\n\
         The literal may still be somewhere in engine.rs — the four presence \
         checks above would still pass — but a live order no longer walks \
         through it. Moving this block into a helper that only the paper path \
         calls is the exact bypass this test was written to reject.\n\n\
         The gate must be in `place_order`'s own body, above the broker call.",
        );

    let cfg = body[..gate].rfind("#[cfg(not(test))]").expect(
        "the sandbox gate inside place_order is no longer wrapped in \
         `#[cfg(not(test))]` — production must always run it",
    );
    assert!(
        gate - cfg < 400,
        "the `#[cfg(not(test))]` nearest the gate is {} bytes above it — too \
         far to be its wrapper. A gate compiled out of production, or wrapped \
         by something else, is not a gate.",
        gate - cfg
    );

    // The refusal must come BEFORE anything that can reach the broker. The
    // token fetch is the first such step; the readiness gate sits between.
    for (needle, what) in [
        ("get_access_token", "the token fetch"),
        ("DhanPlaceOrderRequest", "building the broker request"),
    ] {
        if let Some(at) = body.find(needle) {
            assert!(
                gate < at,
                "the sandbox gate runs AFTER {what} in place_order. A gate \
                 below the network call is a gate the order has already \
                 passed."
            );
        }
    }

    let ret = body[gate..]
        .find("return Err(OmsError::SandboxEnforcement)")
        .expect("the gate inside place_order must REFUSE, not log and continue");
    assert!(
        ret < 800,
        "the gate's refusal is {ret} bytes below its condition — too far to be \
         its body. A condition whose branch does something else is not a gate."
    );
}

/// The rule above, as a pure function, proven against a KNOWN BAD input.
///
/// A guard that has never been shown to FAIL is not a guard, and this one
/// protects the last automatic stop before real money.
#[test]
fn the_structural_rule_rejects_the_hoisted_gate_bypass() {
    fn gate_is_in_body(body: &str) -> bool {
        let Some(gate) = body.find("if now_secs < SANDBOX_DEADLINE_EPOCH_SECS") else {
            return false;
        };
        let Some(cfg) = body[..gate].rfind("#[cfg(not(test))]") else {
            return false;
        };
        if gate - cfg >= 400 {
            return false;
        }
        body[gate..]
            .find("return Err(OmsError::SandboxEnforcement)")
            .is_some_and(|r| r < 800)
    }

    let good = r#"
        #[cfg(not(test))]
        {
            let now_secs = chrono::Utc::now().timestamp();
            if now_secs < SANDBOX_DEADLINE_EPOCH_SECS {
                return Err(OmsError::SandboxEnforcement);
            }
        }
        let token = self.get_access_token().await?;
    "#;
    assert!(gate_is_in_body(good), "the shipped shape must pass");

    // THE BYPASS: the gate has been hoisted into a helper. Every literal the
    // four presence checks look for is still in the FILE — just not here.
    let hoisted = r#"
        self.check_live_order_gates(OrderEndpoint::Place, &correlation_id)?;
        let token = self.get_access_token().await?;
    "#;
    assert!(
        !gate_is_in_body(hoisted),
        "a place_order body with no gate MUST be rejected — this is the exact \
         bypass, and accepting it makes this file decoration"
    );

    // Present but neutered: the condition is there, the refusal is not.
    let neutered = r#"
        #[cfg(not(test))]
        {
            if now_secs < SANDBOX_DEADLINE_EPOCH_SECS {
                metrics::counter!("tv_sandbox_gate_blocks_total").increment(1);
            }
        }
    "#;
    assert!(
        !gate_is_in_body(neutered),
        "a condition that counts but does not refuse must be rejected"
    );
}
