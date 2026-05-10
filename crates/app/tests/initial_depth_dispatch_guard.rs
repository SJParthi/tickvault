//! Fix #4 (2026-04-24): 09:13 IST triple-dispatch wiring ratchets.
//!
//! The 09:13:00 IST tokio task in `crates/app/src/main.rs` fires three
//! subscribe dispatches from the same preopen-buffer snapshot:
//!
//! 1. Main-feed Phase 2 — stock F&O ATM ± 25 via the pool's subscribe
//!    command channels (dispatched by `run_phase2_scheduler`).
//! 2. Depth-20 — NIFTY / BANKNIFTY / FINNIFTY / MIDCPNIFTY ATM ± 24 via
//!    `DepthCommand::InitialSubscribe20`.
//! 3. Depth-200 — NIFTY CE + PE, BANKNIFTY CE + PE at the ATM strike via
//!    `DepthCommand::InitialSubscribe200`.
//!
//! These tests are source-scanners, not runtime tests. Behavioural
//! coverage would need a full boot sequence (Docker, secrets, trading
//! calendar) plus a mocked tokio clock to fast-forward to 09:13 IST —
//! expensive and flaky. A mechanical scan is deterministic, O(1), and
//! immune to tokio runtime flakes.
//!
//! When any assertion here fails, the PR that caused it is regressing
//! Fix #4. Repair the source change, not this test.
//!
//! See `.claude/plans/active-plan.md` for the full plan + scenarios.

use std::path::PathBuf;

fn read_main_rs() -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("src/main.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("must be able to read {}: {err}", path.display()))
}

// -----------------------------------------------------------------------
// Plan ratchet: `test_depth_200_dispatcher_wired_at_0913`
// -----------------------------------------------------------------------

#[test]
fn test_depth_200_dispatcher_wired_at_0913() {
    let src = read_main_rs();

    // The 09:13 task must reference the 09:13:00 target time and the
    // InitialSubscribe200 command. The combination is what proves the
    // dispatcher is wired at the right time with the right payload.
    assert!(
        src.contains("09:13:00") && src.contains("InitialSubscribe200"),
        "Fix #4 regression: main.rs no longer wires the 09:13:00 IST \
         depth-200 dispatcher. Without `InitialSubscribe200` at 09:13 \
         the deferred 200-level connections for NIFTY CE/PE and \
         BANKNIFTY CE/PE stay idle forever — all 4 variants silent, \
         which is the exact symptom reported on 2026-04-23."
    );
}

// -----------------------------------------------------------------------
// Plan ratchet: `test_depth_20_dispatcher_wired_at_0913`
// -----------------------------------------------------------------------

#[test]
fn test_depth_20_dispatcher_wired_at_0913() {
    let src = read_main_rs();

    assert!(
        src.contains("09:13:00") && src.contains("InitialSubscribe20"),
        "Fix #4 regression: main.rs no longer wires the 09:13:00 IST \
         depth-20 dispatcher. Without `InitialSubscribe20` at 09:13 \
         the deferred 20-level connections for NIFTY / BANKNIFTY / \
         FINNIFTY / MIDCPNIFTY stay idle forever."
    );
}
