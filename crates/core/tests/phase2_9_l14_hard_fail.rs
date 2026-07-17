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
//!
//! RETIRED SUITE (dead live-WS sweep stage 1, 2026-07-17 — operator
//! directive via coordinator): the last surviving test,
//! `h1_l14_refusal_calls_process_exit`, died with the module it pinned —
//! `crates/core/src/pipeline/boot_ordering_gate.rs` was DELETED (zero
//! production callers since the PR-C2 lane deletion; the L14
//! `try_authorize_subscribe` wiring left main.rs on 2026-07-13). Its
//! surviving assertion had degraded to a generic "main.rs contains
//! `std::process::exit(1)`" scan that no longer pinned anything
//! L14-specific. The tombstone below keeps the retirement record
//! greppable (the `ip_monitor_wiring_guard.rs` pattern).

#[test]
fn phase2_9_l14_suite_retired_with_boot_ordering_gate() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired and what a future re-wire would have to restore.
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
