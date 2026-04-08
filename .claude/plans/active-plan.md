# Implementation Plan: Depth WS Market-Hours Guard + Materialized View Diagnostic

**Status:** DRAFT
**Date:** 2026-04-08
**Approved by:** pending

## Summary

Fix 2 runtime issues from production logs:
1. Depth WS reconnection storm after market close (590+ attempts per connection)
2. Materialized views not being created — cross-match verification fails

## Plan Items

- [ ] Item 1: Add market-hours guard to 20-level depth reconnection (currently missing)
  - Files: crates/core/src/websocket/depth_connection.rs
  - Change: Add `is_within_market_data_hours()` check in 20-level reconnection loop, sleep until market open when outside hours
  - Tests: test_twenty_depth_market_hours_backoff_constant, test_is_within_market_data_hours_boundary_values

- [ ] Item 2: Improve 200-level depth to sleep until market open (currently 60s retry)
  - Files: crates/core/src/websocket/depth_connection.rs
  - Change: Replace 60s fixed retry with calculated sleep duration to next market window (08:55 IST). Log once.
  - Tests: test_calculate_secs_until_market_open_always_positive, test_calculate_secs_until_market_open_during_hours

- [ ] Item 3: Add materialized view post-setup verification with diagnostic logging
  - Files: crates/storage/src/materialized_views.rs
  - Change: After creating views, verify each exists via `tables()` query. Log ERROR for missing views. This gives clear diagnostics on WHY cross-match fails.
  - Tests: test_verify_views_exist_all_present, test_verify_views_exist_unreachable, test_view_names_for_verification

- [ ] Item 4: Build, test, commit and push
  - Files: all modified
  - Tests: cargo test --workspace

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Market close 15:30 IST, depth WS disconnects | Both 20/200-level log once, sleep until 08:55 next day |
| 2 | Market opens 08:55 IST | Both resume normal exponential backoff reconnection |
| 3 | During market hours, depth WS fails | Normal exponential backoff (unchanged) |
| 4 | Views fail to create at QuestDB | ERROR log with exact missing view names |
| 5 | Views all created successfully | INFO log confirming all 18 views verified |
