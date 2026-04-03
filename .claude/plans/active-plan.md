# Implementation Plan: WebSocket Always-Connect + 9:00 AM Ingestion Gate

**Status:** VERIFIED
**Date:** 2026-04-03
**Approved by:** Parthiban

## Context

WebSocket connections currently only connect during [9:00, 15:30) IST. If the app starts
at 7:30/8:00 AM, WebSockets are skipped entirely. The fix: connect immediately on app
start, but gate ALL tick/depth ingestion to [9:00, 15:30) IST. Order update WS stays
connected until app shutdown (not disconnected at 15:30).

## Plan Items

- [x] P1: Remove `is_within_data_window` guard from WebSocket connection (main.rs)
  - Files: crates/app/src/main.rs
  - Impl: Simplified `should_connect_ws` to `subscription_plan.is_some() && (is_trading || is_mock_trading || is_muhurat)`. Removed `is_within_data_window` computation and stale log message.

- [x] P2: Move ingestion gate BEFORE broadcast/candle/movers in tick processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Impl: Added `is_today_ist()` + `is_within_persist_window()` gate after junk filter, before dedup — applies to both `Tick` and `TickWithDepth` paths. Ticks outside [9:00, 15:30) IST are dropped before dedup ring, greeks enrichment, QuestDB persistence, broadcast, candle aggregation, and top movers. Also gated depth persistence with `continue` for stale/out-of-hours depth.
  - Tests: test_ingestion_gate_pre_market_tick_dropped_before_all_processing, test_ingestion_gate_post_market_tick_dropped_before_all_processing, test_ingestion_gate_last_valid_tick_accepted, test_ingestion_gate_first_valid_tick_accepted + 11 existing boundary tests

- [x] P3: Keep order update WS alive at 15:30 market close (main.rs shutdown)
  - Files: crates/app/src/main.rs
  - Impl: Removed `order_update_handle.abort()` from market close phase (line ~2089). Order update WS now only aborted at final app shutdown (16:00 IST or Ctrl+C).

- [x] P4: Verify fast boot path has no data window guard
  - Files: crates/app/src/main.rs
  - Impl: Confirmed zero references to `is_within_data_window` in entire file. Fast boot connects immediately during market hours. `run_shutdown_fast()` is shared — P3 fix covers both paths.

- [x] P5: Add/update tests for the new behavior
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Impl: Added `today_ist_epoch_at()` test helper for dynamic today+market-hours timestamps. Updated all 80+ test frames from hardcoded `1772073900` to `today_ist_epoch_at(10, 0, 0)`. Added 4 semantic ingestion gate tests.

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
