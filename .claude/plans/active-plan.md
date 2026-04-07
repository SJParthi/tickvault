# Implementation Plan: WebSocket Connection Monitoring — Portal + Telegram + Fixes

**Status:** DRAFT
**Date:** 2026-04-07
**Approved by:** pending

## Summary

1. Portal shows zero WebSocket connection status — need live display for all WS types
2. Telegram alert events for depth/order-update exist in events.rs but are NEVER fired
3. 200-level depth retries endlessly outside market hours (log spam)
4. Historical fetch runs at 8:36 AM (should only run before 8:00 AM or after 15:30)

## Plan Items

- [ ] Item 1: Add depth + order update atomic counters to SystemHealthStatus
  - Files: crates/api/src/state.rs
  - Tests: test_depth_20_connections, test_depth_200_connections, test_order_update_connected

- [ ] Item 2: Expose depth + order update status in /health response
  - Files: crates/api/src/handlers/health.rs
  - Tests: test_health_check_depth_detail, test_health_check_order_update_detail

- [ ] Item 3: Add WebSocket connection status section to portal with live auto-refresh
  - Files: crates/api/static/portal.html
  - Tests: test_portal_html_contains_websocket_status (in static_file.rs)

- [ ] Item 4: Wire Telegram notifications for depth + order update connect/disconnect
  - Files: crates/app/src/main.rs
  - Tests: existing notification event tests cover formatting

- [ ] Item 5: Add market-hours awareness to 200-level depth reconnection
  - Files: crates/core/src/websocket/depth_connection.rs
  - Tests: unit test for pre-market backoff logic

- [ ] Item 6: Fix historical fetch time window (before 8:00 AM OR after 15:30 only)
  - Files: crates/app/src/main.rs
  - Tests: test_historical_fetch_time_guard

- [ ] Item 7: Build and verify all changes
  - Files: n/a
  - Tests: cargo build + cargo test

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Boot at 8:36 AM | Portal shows 5/5 Live Feed, 4/4 depth-20, 0/4 depth-200. Historical fetch SKIPPED (8:00-15:30 window). Telegram: connection alerts for each WS. |
| 2 | Boot at 7:50 AM | Historical fetch runs (before 8:00 cutoff). |
| 3 | Market open 9:15 AM | 200-level depth connects. Portal updates 4/4. Telegram fires connect alerts. |
| 4 | Post-market 15:35 | Historical fetch runs. |
| 5 | WS disconnect mid-market | Portal updates count. Telegram fires DISCONNECTED alert. |
| 6 | 200-level depth pre-market | Logs INFO once, waits until 8:55 AM before retrying. No spam. |
