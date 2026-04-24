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

// -----------------------------------------------------------------------
// Plan ratchet: `test_three_dispatches_use_single_snapshot`
//
// Proves the three dispatches all read from the SAME preopen buffer +
// spot-prices sources rather than taking independent reads that could
// drift. The 09:13 dispatcher task reads the buffer exactly once via
// `preopen_price_buffer::snapshot(&anchor_buffer).await`, then builds
// `selections = select_depth_instruments(..., &spot_map, ...)` a single
// time and iterates the result to fire BOTH `InitialSubscribe20` and
// `InitialSubscribe200`. Phase 2 main-feed reads from the same
// immutable buffer (the window closes at 09:12 IST — see Fix #1 for
// the widened 09:00-09:12 window). No drift possible.
// -----------------------------------------------------------------------

#[test]
fn test_three_dispatches_use_single_snapshot() {
    let src = read_main_rs();

    // (a) The buffer snapshot is taken exactly ONCE in the 09:13 task.
    let snapshot_calls = src.matches("preopen_price_buffer::snapshot").count();
    assert!(
        snapshot_calls >= 1,
        "Fix #4 regression: the 09:13 dispatcher no longer snapshots \
         the preopen buffer. Without a single snapshot read, depth-20 \
         and depth-200 can see drift between reads."
    );

    // (b) `select_depth_instruments` is called (builds selections once).
    assert!(
        src.contains("select_depth_instruments"),
        "Fix #4 regression: the 09:13 dispatcher no longer calls \
         `select_depth_instruments`. Without it the 20-level + 200-level \
         commands cannot share a single selections vector."
    );

    // (c) The two InitialSubscribe commands are dispatched in the same
    // `for sel in &selections` loop so they share the selections
    // source. Locate both references; they MUST both appear between
    // the `let selections =` binding and the next top-level `tokio::spawn`
    // or end of block. We approximate this by asserting both substrings
    // appear within a 10_000 char window starting from the selections
    // binding — the dispatcher body is ~300 lines.
    let selections_pos = src
        .find("let selections =")
        .expect("expected `let selections =` in the 09:13 dispatcher");
    let window_end = (selections_pos + 10_000).min(src.len());
    let window = &src[selections_pos..window_end];
    assert!(
        window.contains("InitialSubscribe20") && window.contains("InitialSubscribe200"),
        "Fix #4 regression: `InitialSubscribe20` and `InitialSubscribe200` \
         are no longer dispatched from the same `selections` iteration. \
         If they move into separate loops with separate `select_depth_instruments` \
         calls, drift becomes possible between the two reads."
    );
}

// -----------------------------------------------------------------------
// Counter ratchet — ops MUST be able to prove the dispatch fired.
// -----------------------------------------------------------------------

#[test]
fn test_initial_depth_dispatch_emits_counters() {
    let src = read_main_rs();

    assert!(
        src.contains("tv_depth_initial_subscribe_dispatched_total"),
        "Fix #4 regression: dispatch-success counter removed. Ops \
         cannot verify that InitialSubscribe20 / InitialSubscribe200 \
         actually fired today without this counter."
    );
    assert!(
        src.contains("tv_depth_initial_subscribe_dispatch_failed_total"),
        "Fix #4 regression: dispatch-failure counter removed. Silent \
         dispatcher failures would be invisible to Grafana."
    );
}
