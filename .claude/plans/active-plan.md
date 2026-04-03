# Implementation Plan: WebSocket Always-Connect + 9:00 AM Ingestion Gate

**Status:** DRAFT
**Date:** 2026-04-03
**Approved by:** pending

## Context

WebSocket connections currently only connect during [9:00, 15:30) IST. If the app starts
at 7:30/8:00 AM, WebSockets are skipped entirely. The fix: connect immediately on app
start, but gate ALL tick/depth ingestion to [9:00, 15:30) IST. Order update WS stays
connected until app shutdown (not disconnected at 15:30).

## Plan Items

- [ ] P1: Remove `is_within_data_window` guard from WebSocket connection (main.rs)
  - Files: crates/app/src/main.rs
  - Tests: test_ws_connects_on_trading_day_regardless_of_time (existing behavior test update)
  - Details: Lines 989-1001 — remove `is_within_data_window` computation, simplify
    `should_connect_ws` to `subscription_plan.is_some() && (is_trading || is_mock_trading || is_muhurat)`.
    Remove the "outside data collection window" log message (no longer applicable).

- [ ] P2: Move ingestion gate BEFORE broadcast/candle/movers in tick processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_tick_outside_market_hours_dropped_before_broadcast,
           test_tick_inside_market_hours_processed,
           test_depth_outside_market_hours_dropped
  - Details: Currently `is_within_persist_window()` only gates QuestDB persistence
    (line 541). Ticks outside [9:00, 15:30) still reach broadcast, candle aggregator,
    and top movers. Move the time+day gate to line ~501 (after junk filter, before dedup)
    so ALL processing is skipped for pre-market stale ticks. Same for TickWithDepth path.
    The existing `is_within_persist_window()` function and constants are reused (O(1),
    zero allocation — 1 modulo + 1 range check).

- [ ] P3: Keep order update WS alive at 15:30 market close (main.rs shutdown)
  - Files: crates/app/src/main.rs
  - Tests: test_order_update_ws_survives_market_close (conceptual — shutdown is integration)
  - Details: Line 2097-2099 in `run_shutdown_fast()` currently aborts `order_update_handle`
    at market close alongside market feed. Remove those 3 lines. The order update handle
    is already correctly aborted at line 2156 (final app shutdown at 16:00 IST).

- [ ] P4: Update fast boot path to remove data window guard
  - Files: crates/app/src/main.rs
  - Tests: (covered by P1 — fast boot also uses `should_connect_ws` equivalent)
  - Details: Fast boot path (lines 295-743) already connects immediately (it only runs
    during market hours). Verify no `is_within_data_window` check exists in fast boot.
    The fast boot `run_shutdown_fast()` is the SAME function as slow boot — P3 fix covers both.

- [ ] P5: Add/update tests for the new behavior
  - Files: crates/core/src/pipeline/tick_processor.rs (unit tests)
  - Tests: test_is_within_persist_window_boundary_9am_inclusive,
           test_is_within_persist_window_boundary_330pm_exclusive,
           test_is_within_persist_window_pre_market_rejected,
           test_is_within_persist_window_post_market_rejected

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App starts at 7:30 AM on trading day | WS connects, ticks dropped until 9:00 |
| 2 | App starts at 10:00 AM on trading day | WS connects, ticks processed immediately |
| 3 | 15:29:59.999 tick arrives | Processed (within window) |
| 4 | 15:30:00.000 tick arrives | Dropped (outside window, exclusive end) |
| 5 | Market close 15:30 | Market feed + depth WS disconnected. Order update WS stays alive |
| 6 | App shutdown 16:00 (or Ctrl+C) | Order update WS disconnected |
| 7 | Crash at 10:15 → fast boot restart | WS reconnects immediately, ticks flow (past 9:00) |
| 8 | Non-trading day (weekend) | WS NOT connected (no change) |
