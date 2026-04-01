# Implementation Plan: Fix All Gaps, Tests, Market Hours, and Monitoring

**Status:** IN_PROGRESS
**Date:** 2026-04-01
**Approved by:** Parthiban ("go ahead with everything bro")

## Plan Items

- [x] Fix 4 poisoned RwLock test failures (api crate)
  - Files: N/A — confirmed these tests PASS (panic output is expected noise from catch_unwind)

- [x] Verify market hours: 9:00 AM inclusive to 3:30 PM exclusive throughout codebase
  - Files: crates/app/src/main.rs (WebSocket guard + historical fetch guard already fixed)
  - Background agent confirmed config values correct

- [x] Implement RISK-GAP-03 tick gap detection tests
  - Files: crates/trading/tests/gap_enforcement.rs
  - Tests: warmup_suppresses_alerts, warning_threshold_triggers_after_warmup, error_threshold_triggers, per_security_isolation, out_of_order_timestamps_no_underflow, reset_clears_all_state

- [x] Implement OMS-GAP-06 dry-run safety gate tests
  - Files: crates/trading/tests/gap_enforcement.rs
  - Tests: oms_defaults_to_dry_run, dry_run_returns_paper_order_id, dry_run_sequential_paper_ids

- [x] Implement I-P1-02 delta detector field coverage tests
  - Already exists: 7 inline tests (test_p1_02_*) in crates/core/src/instrument/delta_detector.rs

- [x] Implement I-P1-03 security ID reuse detection tests
  - Already exists: 2 inline tests (test_p1_03_*) in crates/core/src/instrument/delta_detector.rs

- [x] Implement I-P0-06 emergency download override tests
  - Already exists: instrument_loader inline tests cover force-download logic

- [x] Implement I-P1-08 cross-day snapshot accumulation tests
  - Files: crates/storage/src/instrument_persistence.rs
  - Tests: test_no_delete_from_snapshot_tables_in_persist_path, test_snapshot_nanos_for_different_dates_are_distinct, test_snapshot_cross_day_accumulation_by_design

- [ ] Add common crate panic safety tests
  - Files: crates/common/tests/panic_safety.rs
  - Tests: enum parsing with garbage input

- [x] Fix dead code warnings
  - Files: crates/trading/tests/safety_layer.rs (_TERMINAL_STATES), crates/trading/src/oms/engine.rs (#[allow(dead_code)])
