# Implementation Plan: WebSocket Gap Fixes (C1-C3, H1-H5)

**Status:** APPROVED
**Date:** 2026-04-09
**Approved by:** Parthiban (implicit — "fix everything")

## Plan Items

- [ ] C3: Order update token Zeroizing wrapper
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: existing tests pass

- [ ] H1: WebSocketDisconnected Telegram alert
  - Files: crates/app/src/main.rs (shutdown/reconnect paths)
  - Tests: existing tests pass

- [ ] M5: Redact token from WebSocket URL in error messages
  - Files: crates/core/src/websocket/connection.rs
  - Tests: existing tests pass

- [ ] M7: Order update dropped message metric
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: existing tests pass

- [ ] Update docs with all resolved items
  - Files: docs/architecture/websocket-complete-reference.md

- [ ] Build + clippy + test
  - Tests: cargo test --workspace
