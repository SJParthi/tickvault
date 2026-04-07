# Implementation Plan: Fix Alloy, Indicator Snapshots, WS Status on Dashboard

**Status:** APPROVED
**Date:** 2026-04-07
**Approved by:** Parthiban

## Summary

Fix 4 issues: Alloy DOWN (missing app.log), indicator snapshots only persisting 1 security/60s,
market dashboard missing depth/order WS status, and revert 200-level URL (both paths fail).

## Plan Items

- [ ] Item 1: Fix Alloy — ensure data/logs/app.log exists at boot
  - Files: crates/app/src/main.rs, data/logs/.gitkeep
  - Tests: compilation

- [ ] Item 2: Fix indicator snapshots — persist ALL tracked securities every 60s
  - Files: crates/app/src/trading_pipeline.rs
  - Change: Iterate all indicator engine tracked securities on the 60s timer, not just current tick
  - Tests: existing tests

- [ ] Item 3: Add health/WS status endpoint and show on market dashboard
  - Files: crates/api/static/market-dashboard.html, crates/api/src/handlers/market_data.rs or health.rs
  - Change: Add /api/market/health endpoint returning depth/order WS connection counts, show as status bar
  - Tests: compilation

- [ ] Item 4: Revert 200-level URL to root path (both fail, root is SDK default)
  - Files: crates/common/src/constants.rs, crates/core/src/websocket/depth_connection.rs, crates/common/tests/dhan_api_coverage.rs
  - Tests: existing tests

- [ ] Item 5: Build, test, commit, push
  - Files: n/a
  - Tests: cargo build + cargo test --workspace
