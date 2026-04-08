# Implementation Plan: Production Log Fixes (6 Issues)

**Status:** IN_PROGRESS
**Date:** 2026-04-08
**Approved by:** Parthiban

## Summary

Fix 6 production issues identified from runtime logs and Grafana screenshots:
1. Alloy container DOWN (missing app.log before Docker starts)
2. Historical candles = 0 (pre-market zone catches post-market too)
3. Materialized views all missing (tables() doesn't list materialized views)
4. Grafana missing depth/order WS panels
5. Boot timeout exceeded (constituency downloads block boot for 91s)
6. Cross-verify also uses wrong tables() check

## Plan Items

- [x] Item 1: Fix Alloy DOWN — create data/logs/app.log before docker compose up
  - Files: crates/app/src/infra.rs
  - Change: Added OpenOptions::new().create(true).append(true).open("data/logs/app.log") after create_dir_all

- [x] Item 2: Fix historical candle fetch — narrow pre-market zone
  - Files: crates/app/src/main.rs
  - Change: Changed `hour >= 8` to `(8..16).contains(&hour)` — 16:00+ means post-market, fetch immediately

- [x] Item 3: Fix materialized view verification — use SHOW COLUMNS instead of tables()
  - Files: crates/storage/src/materialized_views.rs
  - Change: Replaced `SELECT count() FROM tables() WHERE name = '...'` with `SHOW COLUMNS FROM <view>` (proven pattern from views_missing_greeks)

- [x] Item 4: Fix cross-verify also uses wrong tables() check
  - Files: crates/core/src/historical/cross_verify.rs
  - Change: Replaced tables() check with SHOW COLUMNS FROM approach

- [x] Item 5: Fix boot timeout — move constituency downloads to background
  - Files: crates/app/src/main.rs
  - Change: Wrapped non-market-hours constituency download in tokio::spawn() (non-blocking). Cache loading during market hours remains synchronous (fast, no network).

- [x] Item 6: Add depth + order update WS metrics and Grafana panels
  - Files: crates/core/src/websocket/order_update_connection.rs, deploy/docker/grafana/dashboards/trading-pipeline.json
  - Change: Added dlt_order_update_ws_active gauge + dlt_order_update_messages_total counter. Added 4 Grafana panels for depth 20/200-level connections, depth frames rate, depth frames dropped.

- [ ] Item 7: Build, clippy, fmt, test — verify all pass
  - Files: all modified
  - Tests: cargo test --workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App starts after 3:30 PM (17:08 IST) | Historical fetch runs immediately (not blocked by post-market signal wait) |
| 2 | App starts at 8:30 AM | Still waits for post-market signal (pre-market zone) |
| 3 | QuestDB materialized views created | SHOW COLUMNS verifies they exist (not tables()) |
| 4 | Constituency download slow (91s) | Background tokio::spawn, boot completes under 120s |
| 5 | Fresh Docker start | app.log exists before Alloy → healthcheck passes → UP |
| 6 | Grafana Trading Pipeline dashboard | Shows depth 20/200-level + order update WS connections |
