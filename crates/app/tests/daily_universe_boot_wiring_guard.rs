//! RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B; operator Q3: "hereafter no Dhan instrument
//! download/parsing — just direct hardcoded security IDs passed to spot 1m
//! and option chain").
//!
//! This guard pinned the main.rs Step-6c wiring of the daily-universe boot
//! orchestrator (`run_daily_universe_boot` + the three lifecycle-table
//! ensures + the fail-closed §4 halt). The Step-6c call sites DIED with the
//! Dhan lane: main.rs performs NO Dhan instrument fetch — the retained Dhan
//! REST pulls run on the HARDCODED `SPOT_1M_REST_INDICES`. The
//! `daily_universe_boot.rs` MODULE is retained un-consumed pending the
//! Phase C deletion of the daily-universe fetch chain (scope-lock amendment
//! §B item 3); its own in-module tests still compile it. The
//! `instrument_lifecycle` / `instrument_lifecycle_audit` /
//! `instrument_fetch_audit` TABLES survive (SEBI never-delete) and are
//! ensured/written by the Groww shared-master writer
//! (`groww-shared-master-error-codes.md`).

#[test]
fn daily_universe_boot_wiring_suite_retired_with_dhan_live_ws_lane() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired. Re-introducing a Dhan instrument download requires
    // a fresh dated operator quote in websocket-connection-scope-lock.md
    // FIRST (§D of the amendment).
}
