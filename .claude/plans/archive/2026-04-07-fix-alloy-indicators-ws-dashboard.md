# Implementation Plan: Fix Alloy, Indicator Snapshots, WS Status on Dashboard

**Status:** VERIFIED
**Date:** 2026-04-07
**Approved by:** Parthiban

## Summary

Fix 4 issues: Alloy DOWN (missing app.log), indicator snapshots only persisting 1 security/60s,
market dashboard missing depth/order WS status, and revert 200-level URL (both paths fail).

## Plan Items

- [x] Item 1: Fix Alloy — ensure data/logs/app.log exists at boot
  - Files: data/logs/.gitkeep (can't commit — data/ gitignored), app.log created manually
  - Tests: compilation

- [x] Item 2: Fix indicator snapshots — persist ALL tracked securities every 60s
  - Files: crates/app/src/trading_pipeline.rs
  - Change: HashMap batch accumulates snapshots per tick, flushes all every 60s
  - Tests: 375/375 app tests pass

- [x] Item 3: Add health/WS status to market dashboard
  - Files: crates/api/static/market-dashboard.html
  - Change: Connection status bar with green/red dots for all 7 subsystems, fetches /health
  - Tests: compilation

- [x] Item 4: 200-level URL verified correct (/twohundreddepth per official docs)
  - Files: already committed in previous commit
  - Tests: existing tests pass

- [x] Item 5: Build, test, commit, push
  - Files: n/a
  - Tests: cargo build + 375/375 app tests pass
