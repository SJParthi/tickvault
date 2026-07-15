//! Phase 2.12 — hostile bug-hunt L1 fix: fast/slow boot mode
//! visibility via Prometheus gauge.
//!
//! What was missing: fast boot passes `None` for `tick_enricher` (no
//! lifecycle column population — recovery path). Slow boot passes
//! `Some(enricher)` (full lifecycle population). If an operator
//! switches between modes via process restart, the change was only
//! visible in the boot log lines — not in Prometheus history,
//! not in dashboards.
//!
//! The fix: emit `tv_lifecycle_enricher_attached{boot_mode="fast"|"slow"}`
//! gauge at boot:
//!   - fast boot: value 0.0 (lifecycle columns NOT populated)
//!   - slow boot: value 1.0 (lifecycle columns populated)
//!
//! Operators can:
//!   - Chart `tv_lifecycle_enricher_attached` over time to see mode
//!     transitions
//!   - Alert on transitions: a fast boot during business hours is a
//!     warning signal (recovery mode active)
//!   - Correlate with QuestDB column population — value=0 + 0 lifecycle
//!     rows is the expected fast-boot contract; value=1 + 0 lifecycle
//!     rows is a real bug
//!
//! Plus structured INFO log lines at both sites for human readability.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_main_rs_emits_lifecycle_enricher_attached_gauge died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_fast_boot_sets_gauge_to_zero died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_slow_boot_sets_gauge_to_one died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment) was deleted from main.rs with the lane.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_fast_boot_logs_enricher_attached_false died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_slow_boot_logs_enricher_attached_true died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_fast_boot_log_explains_lifecycle_columns_will_not_be_populated died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md
// "2026-07-13 Amendment" §B): phase2_12_slow_boot_log_lists_attached_subsystems died with the wiring it pinned — the
// Dhan tick pipeline (frame channel → run_tick_processor → TickEnricher /
// prev_oi lifecycle enrichment / L14 boot-ordering gate) was deleted from
// main.rs with the lane; the Groww feed carries its own bridge path.
