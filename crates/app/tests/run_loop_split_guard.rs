//! D2 run-loop guard — re-shaped by PR-C2 (2026-07-13, operator retirement
//! directive per websocket-connection-scope-lock.md "2026-07-13 Amendment"
//! §B).
//!
//! The original D2 Stage-1 ratchet pinned the `run_process_runloop` /
//! `teardown_dhan_lane_tasks` SPLIT (so D2b's runtime `stop_dhan_lane`
//! could reuse a clean lane-only teardown). The LANE half —
//! `teardown_dhan_lane_tasks`, `DhanLaneRunHandles`, and the
//! `Option<DhanLaneRunHandles>` run-loop parameter — was DELETED with the
//! Dhan live-WS lane (there is no lane to tear down; the order-update WS
//! is owned by dhan_rest_stack). Those pins are RETIRED.
//!
//! What SURVIVES (pinned below): `run_process_runloop` remains the single
//! PROCESS run-loop — market-close timer, post-market partition detach,
//! shutdown-signal wait, then API + otel teardown — shared by every boot
//! (Groww-only included).

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).expect("crates/app/src/main.rs must be readable")
}

/// Returns the body text of `async fn <name>(` up to (but not including) the
/// next top-level declaration.
fn fn_body(src: &str, signature: &str) -> String {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` must exist in main.rs"));
    let rest = &src[start + signature.len()..];
    let end = ["\nasync fn ", "\nfn ", "\nstruct ", "\n#[allow"]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn run_process_runloop_exists_and_owns_the_process_concerns() {
    let src = main_rs();
    assert!(
        src.contains("async fn run_process_runloop("),
        "the PROCESS run-loop MUST stay its own `run_process_runloop` fn — \
         every boot (Groww-only included) shares it."
    );
    let body = fn_body(&src, "async fn run_process_runloop(");
    for needle in [
        // Shutdown-signal wait (SIGTERM/SIGINT + market-close routing).
        "wait_for_shutdown_signal()",
        // Post-market QuestDB partition detach (Phase B retention).
        "detach_old_partitions()",
    ] {
        assert!(
            body.contains(needle),
            "run_process_runloop MUST keep `{needle}` — dropping it \
             re-introduces a shutdown / retention regression."
        );
    }
}

// RETIRED (PR-C2, 2026-07-13): `run_loop_and_lane_teardown_are_split`,
// `run_process_runloop_takes_optional_lane_handles`,
// `lane_teardown_does_not_run_process_only_concerns`, and
// `lane_teardown_keeps_the_load_bearing_shutdown_steps` pinned
// `teardown_dhan_lane_tasks` / `DhanLaneRunHandles` /
// `Option<DhanLaneRunHandles>` — all deleted with the Dhan live-WS lane.
// The SIGTERM data-loss guards those teardown steps protected
// (`request_graceful_shutdown` RequestCode-12, `supervise_pool` drain,
// pool-watchdog notify) died with the pool itself; the surviving Dhan WS
// (order-update, functional-dormant) is torn down by dhan_rest_stack's
// own task ownership.
