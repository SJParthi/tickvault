//! Phase 2.6 — production-attach ratchet for the TickEnricher.
//!
//! Source-scan tests that pin the boot-time wiring in
//! `crates/app/src/main.rs`. Slow boot MUST:
//!  (a) construct a `TickEnricher`,
//!  (b) call `prev_oi_cache.load_from_questdb(...)` and handle the
//!      Result (graceful degradation on failure),
//!  (c) mark the three L14 boot phases on a `BootOrderingGate`,
//!  (d) pass `Some(Arc::clone(&tick_enricher))` to `run_tick_processor`
//!      (NOT None — that's the legacy unattached state).
//!
//! Drift in any of these surfaces here as a build-fail rather than
//! silent regression to the legacy zero-default lifecycle path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): slow_boot_constructs_tick_enricher died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): slow_boot_loads_prev_oi_cache_from_questdb died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): slow_boot_handles_prev_oi_cache_load_error_fail_closed died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): slow_boot_marks_boot_ordering_gate_phases died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): slow_boot_passes_some_tick_enricher_to_run_tick_processor died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment) was deleted from main.rs with the lane.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): fast_boot_still_passes_none_tick_enricher died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): slow_boot_logs_l14_authorization died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.
