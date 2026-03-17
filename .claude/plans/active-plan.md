# Implementation Plan: Guarantee Tick Persistence Bounds (9:00–15:30 IST)

**Status:** DRAFT
**Date:** 2026-03-17
**Approved by:** pending

## Plan Items

- [ ] P1: Add market-hours tick persistence constants
  - Files: crates/common/src/constants.rs
  - Add `TICK_PERSIST_START_SECS_OF_DAY_IST = 32_400` (09:00:00 = 9×3600)
  - Add `TICK_PERSIST_END_SECS_OF_DAY_IST = 55_800` (15:30:00 = 15×3600 + 30×60)
  - Add `SECONDS_PER_DAY: u32 = 86_400`
  - Add `MARKET_CLOSE_DRAIN_BUFFER_SECS: u64 = 2`
  - Tests: test_tick_persist_start_matches_nine_am, test_tick_persist_end_matches_three_thirty, test_seconds_per_day_correct

- [ ] P2: Add O(1) inline tick timestamp filter in tick_processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Add `#[inline(always)] fn is_within_persist_window(exchange_timestamp: u32) -> bool`
    - `(exchange_timestamp + IST_UTC_OFFSET_SECONDS as u32) % SECONDS_PER_DAY` → check `[32400, 55800)`
  - Guard tick persistence (line 238) with `is_within_persist_window(tick.exchange_timestamp)`
  - Guard tick-with-depth persistence (line 302) with same check
  - Guard depth persistence (line 358) with same check
  - Guard candle persistence — periodic flush (line 494) filter candles by timestamp
  - Guard candle persistence — final flush (line 556) filter candles by timestamp
  - Add `m_outside_hours_filtered` counter metric + `outside_hours_filtered` local counter
  - Tests: test_persist_window_market_open_boundary, test_persist_window_market_close_boundary, test_persist_window_pre_market_rejected, test_persist_window_post_market_rejected, test_persist_window_midnight_rejected

- [ ] P3: Fix WS disconnect to drain before abort
  - Files: crates/app/src/main.rs
  - In `run_shutdown_fast`, after market_close fires:
    - Sleep `MARKET_CLOSE_DRAIN_BUFFER_SECS` (2s) FIRST to let in-flight ticks arrive
    - THEN abort WS handles
    - THEN sleep `GRACEFUL_SHUTDOWN_TIMEOUT_SECS` for tick processor drain
  - This guarantees the last 15:29 candle ticks are in the channel before WS abort
  - Tests: test_market_close_drain_buffer_constant

- [ ] P4: Fix `data_collection_end` config inconsistency
  - Files: config/base.toml
  - Change `data_collection_end = "16:00:00"` → `"15:30:00"` to match actual WS disconnect and persist bounds
  - Also update any test configs that reference the old value

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Pre-market tick at 08:59:59 IST arrives | NOT persisted to QuestDB (filtered by `is_within_persist_window`) |
| 2 | Market open tick at 09:00:00 IST arrives | Persisted (within window) |
| 3 | Last valid tick at 15:29:59 IST arrives | Persisted (within window) |
| 4 | Post-market tick at 15:30:00 IST arrives | NOT persisted (outside window) |
| 5 | WS disconnect at 15:30 — last tick at 15:29:58 | 2s drain buffer ensures tick reaches channel before abort |
| 6 | Candle with timestamp at 08:55 IST | NOT persisted to QuestDB |
| 7 | Candle with timestamp at 15:29 IST | Persisted to QuestDB |
| 8 | Slow boot before 09:00 — WS+writer both active | Pre-market ticks flow to candle aggregator but NOT to QuestDB |
