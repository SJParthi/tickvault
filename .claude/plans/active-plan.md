# Implementation Plan: Depth WS Market-Hours Guard + Materialized View Diagnostic + Frontend Fix

**Status:** IN_PROGRESS
**Date:** 2026-04-08
**Approved by:** Parthiban

## Summary

Fix 3 issues from production logs and frontend audit:
1. Depth WS reconnection storm after market close (590+ attempts per connection)
2. Materialized views diagnostic — clear ERROR logging when cross-match fails
3. Frontend: option-chain-v2.html orphaned (not routed)

## Plan Items

- [x] Item 1: Add market-hours guard to 20-level depth reconnection (currently missing)
  - Files: crates/core/src/websocket/depth_connection.rs
  - Change: Added `is_within_market_data_hours()` check + `calculate_secs_until_market_open()` in 20-level reconnection loop
  - Tests: test_is_within_market_data_hours_returns_bool, test_is_within_market_data_hours_boundary_constants, test_calculate_secs_until_market_open_always_positive, test_calculate_secs_until_market_open_bounded, test_off_hours_check_interval_reasonable

- [x] Item 2: Improve 200-level depth to sleep until market open (was 60s retry)
  - Files: crates/core/src/websocket/depth_connection.rs
  - Change: Replaced 60s fixed retry with `calculate_secs_until_market_open()` (5min check interval, up to 24h sleep)
  - Tests: (covered by same tests as Item 1 — shared helper function)

- [x] Item 3: Add materialized view post-setup verification with diagnostic logging
  - Files: crates/storage/src/materialized_views.rs
  - Change: Added `verify_views_exist()` with per-view existence check, ERROR for critical missing views
  - Tests: test_cross_match_critical_views_count, test_cross_match_critical_views_are_defined, test_cross_match_critical_views_exact_names, test_verify_views_exist_with_mock_200_no_panic, test_verify_views_exist_with_mock_400_no_panic, test_verify_views_exist_with_unreachable_no_panic, test_verify_views_exist_with_tracing

- [x] Item 4: Route option-chain-v2.html (orphaned file from frontend audit)
  - Files: crates/api/src/handlers/static_file.rs, crates/api/src/lib.rs
  - Change: Added OPTIONS_CHAIN_V2_HTML constant, options_chain_v2() handler, GET /portal/option-chain-v2 route
  - Tests: test_options_chain_v2_html_not_empty, test_options_chain_v2_handler_returns_ok, test_options_chain_v2_handler_content_type_is_html, test_options_chain_v2_html_has_closing_tags, test_options_chain_v2_html_contains_option_chain_endpoint

- [x] Item 5: Fix pre-existing clippy warning
  - Files: crates/storage/src/candle_persistence.rs
  - Change: Removed needless `return` keyword (clippy::needless_return)

- [x] Item 6: Build, clippy, fmt, test — all pass
  - Files: all modified
  - Tests: cargo build + cargo clippy + cargo fmt --check + cargo test (all pass)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Market close 15:30 IST, depth WS disconnects | Both 20/200-level log once, sleep ~5min intervals until 08:55 next day |
| 2 | Market opens 08:55 IST | Both resume normal exponential backoff reconnection |
| 3 | During market hours, depth WS fails | Normal exponential backoff (unchanged) |
| 4 | Views fail to create at QuestDB | ERROR log with exact missing view names + critical flag |
| 5 | Views all created successfully | INFO log confirming all 18 views verified |
| 6 | option-chain-v2.html accessed | Served at /portal/option-chain-v2 with correct content-type |
