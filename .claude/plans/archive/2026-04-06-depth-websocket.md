# Implementation Plan: 20-Level Depth WebSocket Connection

**Status:** IN_PROGRESS
**Date:** 2026-04-06
**Approved by:** Parthiban ("go ahead with everything")

## Plan Items

- [x] Add config fields (enable_twenty_depth, twenty_depth_max_instruments)
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: existing config test coverage

- [ ] Create depth_connection.rs module for 20-level depth WebSocket
  - Files: crates/core/src/websocket/depth_connection.rs, crates/core/src/websocket/mod.rs
  - Tests: unit tests for URL construction, subscription building

- [ ] Wire depth connection into boot sequence (main.rs)
  - Files: crates/app/src/main.rs
  - Tests: existing boot path tests

- [ ] Add QuestDB persistence for 20-level depth in tick_processor.rs
  - Files: crates/core/src/pipeline/tick_processor.rs, crates/storage/src/tick_persistence.rs
  - Tests: existing depth persistence tests

- [ ] Commit and push
  - Files: all modified
  - Tests: cargo clippy, cargo test -p dhan-live-trader-core

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | enable_twenty_depth=true | Depth WS connects, frames arrive in tick processor |
| 2 | enable_twenty_depth=false | No depth connection, no error |
| 3 | Depth WS disconnects (805) | Log warning, reconnect with backoff |
| 4 | Deep depth frames parsed | 20 levels per side persisted to QuestDB |
