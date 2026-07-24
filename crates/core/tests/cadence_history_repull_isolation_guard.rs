//! Ratchet: the cadence history re-pull module stays ISOLATED from the
//! per-cycle decision/assembly/audit machinery (PR #1693 item 5).
//!
//! The T+30s/T+50s background re-pull is a fire-and-forget recovery lane:
//! it must never reach back into cycle assembly, decision state, or the
//! cross-fill audit surface (audit rows are sibling-owned). Test A bans
//! those couplings; Test B positively pins the module's own surface so the
//! ban cannot pass vacuously; Test C pins the runner wiring shape.

const MODULE_SRC: &str = include_str!("../src/cadence/history_repull.rs");
const RUNNER_SRC: &str = include_str!("../src/cadence/runner.rs");

#[test]
fn history_repull_module_never_touches_cycle_state() {
    let banned = [
        "assembly::",
        "decision::",
        "audit::",
        "CrossFillAuditEvent",
        "emit_cross_fill_audit",
        "may_retry_in_cycle",
        "insert_event",
        "finalize_if_complete",
        "record_chain",
        "record_spot",
        "LaneRun",
        "CycleAction",
    ];
    for needle in banned {
        assert!(
            !MODULE_SRC.contains(needle),
            "history_repull.rs must stay isolated from cycle state; found banned token: {needle}"
        );
    }
}

#[test]
fn history_repull_module_surface_pinned() {
    let required = [
        "tv_cadence_history_repull_total",
        "CADENCE_HISTORY_REPULL_OFFSETS_MS",
        "CADENCE_HISTORY_REPULL_TIMEOUT_MS",
        "gate_skipped",
        "aborted_429",
        "try_acquire_chain",
        "try_acquire_spot",
    ];
    for needle in required {
        assert!(
            MODULE_SRC.contains(needle),
            "history_repull.rs lost a pinned surface token: {needle}"
        );
    }
}

#[test]
fn runner_wires_history_repull_spawn() {
    assert_eq!(
        RUNNER_SRC.matches("fn spawn_history_repull").count(),
        1,
        "runner.rs must define exactly one spawn_history_repull"
    );
    assert!(
        RUNNER_SRC.contains("history_repull::run_history_repull"),
        "spawn_history_repull must delegate to history_repull::run_history_repull"
    );
    assert!(
        RUNNER_SRC.contains("history_repull_enabled"),
        "the E6 seam must stay config-gated on history_repull_enabled"
    );
}
