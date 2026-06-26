//! D2 genuine-hoist Stage 1 ratchet — the run-loop / lane-teardown split.
//!
//! The D2 design (active-plan-dhan-cold-start-d2.md §1.4, finding C3) requires
//! the old monolithic `run_shutdown_fast` to be split into:
//!
//!   * `run_process_runloop(...)` — the PROCESS run-loop (market-close timer,
//!     partition-detach, shutdown wait, then API + otel teardown). It takes an
//!     `Option<DhanLaneRunHandles>` so the Dhan-OFF / Groww-only case (`None`)
//!     keeps the shared infra alive with NO Dhan-specific teardown.
//!   * `teardown_dhan_lane_tasks(...)` — the LANE-only teardown (renewal →
//!     order-update → graceful WS close → tick-processor flush → trading
//!     pipeline). This is exactly what D2b's runtime `stop_dhan_lane` will call
//!     for the ON→OFF path WITHOUT touching the PROCESS run-loop / partition
//!     detach / `wait_for_shutdown_signal`.
//!
//! This is a SOURCE-SCAN ratchet (same pattern as the sibling boot guards): it
//! fails the build if a future edit re-merges the two so D2b can no longer reuse
//! a clean lane-only teardown, OR lets the lane teardown reach the
//! PROCESS-only run-loop concerns (partition detach / shutdown wait).

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).expect("crates/app/src/main.rs must be readable")
}

/// Returns the body text of `async fn <name>(` up to (but not including) the
/// next top-level `\nasync fn ` / `\nfn ` / `\nstruct ` declaration.
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
fn run_loop_and_lane_teardown_are_split() {
    let src = main_rs();
    assert!(
        src.contains("async fn run_process_runloop("),
        "D2 C3: the PROCESS run-loop MUST be its own `run_process_runloop` fn so \
         a Dhan-OFF / Groww-only runtime (no lane handles) and the Dhan-ON-slow \
         path share ONE run-loop."
    );
    assert!(
        src.contains("async fn teardown_dhan_lane_tasks("),
        "D2 C3: the LANE-only teardown MUST be its own `teardown_dhan_lane_tasks` \
         fn so D2b's runtime `stop_dhan_lane` can reuse it WITHOUT the PROCESS \
         run-loop / partition-detach / shutdown-wait."
    );
    assert!(
        src.contains("struct DhanLaneRunHandles"),
        "D2 C3: the Dhan-lane runtime handles MUST be bundled in \
         `DhanLaneRunHandles` so they can be torn down as a unit (Some) or \
         skipped entirely (None) on a Dhan-OFF runtime."
    );
}

#[test]
fn run_process_runloop_takes_optional_lane_handles() {
    let src = main_rs();
    // The run-loop must accept an Option so the Dhan-OFF / Groww-only case
    // (None) keeps shared infra alive with no Dhan teardown.
    let sig_region = {
        let start = src
            .find("async fn run_process_runloop(")
            .expect("run_process_runloop must exist");
        &src[start..start + 600]
    };
    assert!(
        sig_region.contains("Option<DhanLaneRunHandles>"),
        "D2 C3: `run_process_runloop` MUST take `Option<DhanLaneRunHandles>` so a \
         Dhan-OFF / Groww-only runtime passes `None` (no Dhan-specific teardown) \
         while the shared infra (API + otel) is still torn down."
    );
}

#[test]
fn lane_teardown_does_not_run_process_only_concerns() {
    // C3: the lane teardown must NEVER run the market-close timer,
    // partition-detach, or `wait_for_shutdown_signal` — those are PROCESS
    // run-loop concerns. If they leak into the lane teardown, D2b's runtime
    // `stop_dhan_lane` would wrongly wait for a shutdown signal / detach
    // partitions on a mere lane stop.
    let src = main_rs();
    let body = fn_body(&src, "async fn teardown_dhan_lane_tasks(");
    for forbidden in [
        "wait_for_shutdown_signal",
        "detach_old_partitions",
        "compute_market_close_sleep",
    ] {
        assert!(
            !body.contains(forbidden),
            "D2 C3: `teardown_dhan_lane_tasks` MUST NOT contain `{forbidden}` — that \
             is a PROCESS run-loop concern. The lane teardown is renewal → \
             order-update → graceful WS close → processor flush → trading ONLY."
        );
    }
}

#[test]
fn lane_teardown_keeps_the_load_bearing_shutdown_steps() {
    // The split must preserve the exact lane-teardown steps (the SIGTERM
    // data-loss guards from audit finding #12 / WS-GAP-05).
    let src = main_rs();
    let body = fn_body(&src, "async fn teardown_dhan_lane_tasks(");
    for needle in [
        "shutdown_notify.notify_waiters()", // pool watchdog stop
        "request_graceful_shutdown",        // RequestCode 12 to Dhan
        "supervise_pool",                   // WS-GAP-05 drain
    ] {
        assert!(
            body.contains(needle),
            "D2 C3: `teardown_dhan_lane_tasks` MUST keep `{needle}` — dropping it \
             re-introduces a SIGTERM data-loss / false-Halt regression."
        );
    }
}
