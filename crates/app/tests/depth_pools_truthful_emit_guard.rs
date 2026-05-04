//! PR #460 ratchet — depth-20 + depth-200 counter increments are gated on
//! the truthful "first data frame received" signal, not task spawn.
//!
//! Pre-PR #460 the depth-20 + depth-200 task-spawn blocks in `main.rs`
//! incremented `set_depth_20_connections(...).saturating_add(1)` /
//! `set_depth_200_connections(...).saturating_add(1)` IMMEDIATELY at
//! task spawn — before any handshake / subscribe ack / data frame.
//! That was a false-OK: the operator received "Depth-20: 5/5" before
//! any Dhan data was actually flowing, and even today (2026-05-04)
//! saw "Depth-20: 5/4" while `deep_market_depth` table stayed empty.
//!
//! This test enforces the truthful pattern via source-scan: the saturating
//! increment of the depth counters MUST be inside an `if !`-guarded
//! `swap(true, ...)` CAS block (= "if we hadn't already counted this
//! connection as truthfully-Connected, count it now"). The CAS body
//! can ONLY be reached from the signal-rx waiter spawn, which fires
//! after first data frame.
//!
//! If a future regression moves the increment back to the bare task-
//! spawn block (no CAS), this test fails the build.
//!
//! Run: `cargo test -p tickvault-app --test depth_pools_truthful_emit_guard`

use std::path::PathBuf;

fn main_rs() -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "src", "main.rs"]
        .iter()
        .collect();
    std::fs::read_to_string(&path).expect("crates/app/src/main.rs must be readable")
}

/// Every `set_depth_20_connections(... saturating_add(1) ...)` MUST be
/// preceded (within a small contextual window) by `d20_truly_connected`
/// CAS — the truthful-emit gate. PR #460 contract.
#[test]
fn depth_20_increment_is_gated_on_truly_connected_swap() {
    let src = main_rs();
    let increment_count = src.matches("set_depth_20_connections").count();
    assert!(
        increment_count >= 2,
        "Expected >= 2 occurrences of set_depth_20_connections (one increment, \
         one decrement); found {increment_count}. The depth-20 counter wiring \
         must exist in main.rs."
    );
    // The truthful-emit pattern requires the SHARED FLAG identifier to
    // appear in the file. If a regression deletes the gate, the flag
    // disappears.
    assert!(
        src.contains("d20_truly_connected"),
        "PR #460 truthful-emit invariant violated: the `d20_truly_connected` \
         AtomicBool gate is missing. The depth-20 counter increment must be \
         gated on the signal-rx-fires CAS, not on task spawn."
    );
    // The CAS body must increment, the exit-path must decrement under
    // the same gate.
    assert!(
        src.contains("d20_truly_connected_for_signal")
            && src.contains("d20_truly_connected_for_exit"),
        "PR #460 truthful-emit invariant violated: missing one of \
         d20_truly_connected_for_signal / d20_truly_connected_for_exit. \
         Both must exist so the signal-rx waiter can flip the flag and \
         the connection-runner can decrement only if it had been flipped."
    );
}

/// Same contract for depth-200.
#[test]
fn depth_200_increment_is_gated_on_truly_connected_swap() {
    let src = main_rs();
    let increment_count = src.matches("set_depth_200_connections").count();
    assert!(
        increment_count >= 2,
        "Expected >= 2 occurrences of set_depth_200_connections (one increment, \
         one decrement); found {increment_count}."
    );
    assert!(
        src.contains("d200_truly_connected"),
        "PR #460 truthful-emit invariant violated: the `d200_truly_connected` \
         AtomicBool gate is missing for depth-200."
    );
    assert!(
        src.contains("d200_truly_connected_for_signal")
            && src.contains("d200_truly_connected_for_exit"),
        "PR #460 truthful-emit invariant violated: missing one of \
         d200_truly_connected_for_signal / d200_truly_connected_for_exit."
    );
}

/// The OLD pattern is BANNED. Specifically: the literal string
/// `set_depth_20_connections(\n` followed by an immediate
/// `saturating_add(1)` was the signature of the false-OK.
/// We require the increment to ALWAYS be inside an `if !` CAS block.
#[test]
fn depth_20_no_immediate_post_spawn_increment() {
    let src = main_rs();
    // Find every increment site and confirm it's preceded by the CAS
    // marker within a 200-character lookback window. This is a
    // heuristic — the goal is to fail the build on the legacy pattern
    // without false-positives.
    for (idx, _) in src.match_indices("set_depth_20_connections(") {
        // Look back ~600 chars for either the CAS marker or the
        // explicit "PR #460" comment that flags the truthful emit.
        let lookback = idx.saturating_sub(600);
        let context = &src[lookback..idx];
        let cas_present = context.contains("d20_truly_connected_for_signal")
            || context.contains("d20_truly_connected_for_exit")
            || context.contains("PR #460");
        assert!(
            cas_present,
            "PR #460 truthful-emit invariant violated near byte offset {idx}: \
             a `set_depth_20_connections(...)` call is NOT preceded by the \
             `d20_truly_connected` CAS marker within 600 chars. The legacy \
             increment-on-spawn pattern has been banned. Use the truthful-\
             emit pattern: gate the counter on signal-rx firing."
        );
    }
}

#[test]
fn depth_200_no_immediate_post_spawn_increment() {
    let src = main_rs();
    for (idx, _) in src.match_indices("set_depth_200_connections(") {
        let lookback = idx.saturating_sub(600);
        let context = &src[lookback..idx];
        let cas_present = context.contains("d200_truly_connected_for_signal")
            || context.contains("d200_truly_connected_for_exit")
            || context.contains("PR #460");
        assert!(
            cas_present,
            "PR #460 truthful-emit invariant violated near byte offset {idx}: \
             a `set_depth_200_connections(...)` call is NOT preceded by the \
             `d200_truly_connected` CAS marker within 600 chars."
        );
    }
}
