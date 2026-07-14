//! Phase 2.9 — H1 fix: BootOrderingGate authorization failure now
//! HARD-FAILS the boot via `process::exit(1)` instead of logging
//! ERROR + continuing. The H4 fix from Phase 2.8 already arranged
//! for `mark_oi_cache_loaded` to NOT fire on a `prev_oi_cache.load_from_questdb`
//! error; this PR turns that signal into a binary-level L14
//! enforcement: the process refuses to subscribe with unhealthy state.
//!
//! systemd / docker restart policy handles the recovery loop. The
//! `TICKVAULT_BOOT_DRY_RUN` env var lets tests exercise the branch
//! without killing the runner.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    me.parent()
        .expect("tickvault root")
        .parent()
        .expect("workspace root")
        .to_path_buf()
        .join("crates")
}

fn read_main() -> String {
    let p = workspace_root().join("app/src/main.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|err| panic!("read {p:?}: {err}"))
}

#[test]
fn h1_l14_refusal_calls_process_exit() {
    let src = read_main();
    assert!(
        src.contains("std::process::exit(1)"),
        "main.rs must call std::process::exit(1) when boot_ordering_gate.try_authorize_subscribe \
         returns false (Phase 2.9 H1 fix — binary-level L14 enforcement)"
    );
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h1_l14_refusal_logs_typed_error_code died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h1_l14_refusal_emits_counter died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h1_dry_run_env_var_gate_lets_tests_exercise_path died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): h1_log_level_is_error_not_warn died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.
