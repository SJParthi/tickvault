# Implementation Plan: Block A — Critical Resilience & O(1) Fixes

**Status:** APPROVED
**Date:** 2026-04-03
**Approved by:** Parthiban (approved "starting block a implement everything")

## Plan Items

- [ ] A1: Flush tick ring buffer + candle ring buffer on graceful shutdown
  - Files: crates/core/src/pipeline/tick_processor.rs, crates/storage/src/tick_persistence.rs
  - Tests: test_graceful_shutdown_flushes_ring_buffer, test_graceful_shutdown_flushes_candle_buffer

- [ ] A2: Add Telegram alert when ticks_dropped_total > 0
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_tick_drop_fires_critical_metric

- [ ] A3: Add Telegram alert when ring buffer > 80% (240K ticks)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_buffer_high_watermark_alert

- [ ] A4: Add disk space check before first spill write
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_disk_space_check_before_spill

- [ ] A5: Replace Box<dyn GreeksEnricher> with enum for hot path
  - Files: crates/common/src/tick_types.rs, crates/core/src/pipeline/tick_processor.rs, crates/trading/src/greeks/inline_computer.rs, crates/app/src/main.rs
  - Tests: test_greeks_enricher_enum_dispatch

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App shutdown with 100 ticks in ring buffer | All 100 flushed before exit |
| 2 | Ring buffer hits 240K (80%) | WARN log fires (triggers Telegram via Loki) |
| 3 | ticks_dropped_total goes from 0 to 1 | ERROR log fires (triggers Telegram) |
| 4 | Disk has <100MB before spill | WARN log fires, spill continues (best-effort) |
| 5 | Greeks enricher called on hot path | No vtable indirection, enum match instead |
