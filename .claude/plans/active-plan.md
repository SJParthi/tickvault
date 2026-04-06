# Implementation Plan: Fix Empty Tables (stock_movers, option_movers, indicator_snapshots)

**Status:** VERIFIED
**Date:** 2026-04-06
**Approved by:** pending

## Plan Items

- [x] Fix TopMoversTracker to use prev_close from PrevClose packets instead of tick.day_close
  - Files: crates/core/src/pipeline/top_movers.rs, crates/core/src/pipeline/tick_processor.rs
  - Tests: test_update_prev_close_used_for_change_pct, test_update_without_prev_close_skipped, test_prev_close_from_separate_map

- [x] Fix OptionMoversTracker to use prev_close from PrevClose packets instead of tick.day_close
  - Files: crates/core/src/pipeline/option_movers.rs, crates/core/src/pipeline/tick_processor.rs
  - Tests: test_update_prev_close_from_map, test_update_without_prev_close_uses_zero

- [x] Wire IndicatorSnapshotWriter into trading_pipeline to persist indicator snapshots every 60 seconds
  - Files: crates/app/src/trading_pipeline.rs, crates/app/src/main.rs
  - Tests: test_indicator_snapshot_writer_creation, test_trading_pipeline_config_has_indicator_writer

- [x] Add flush on shutdown for stock_movers_writer and option_movers_writer in tick_processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: existing shutdown tests cover this path

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | PrevClose packet arrives → TopMoversTracker gets prev_close | change_pct computed correctly |
| 2 | Tick arrives without PrevClose received → TopMoversTracker | Tick skipped (no prev_close baseline) |
| 3 | During market hours with F&O subscription | stock_movers + option_movers tables populated |
| 4 | IndicatorEngine computes snapshot → writer persists | indicator_snapshots table populated |
| 5 | App shutdown → movers writers flushed | No data loss on graceful exit |
