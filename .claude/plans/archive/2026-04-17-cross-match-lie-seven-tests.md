# Implementation Plan: Fix Cross-Match Lie + 7 Failing Tests

**Status:** VERIFIED
**Date:** 2026-04-17
**Approved by:** Parthiban ("fix everything dude")

## Plan Items

- [x] 1. Cross-match pre-check for live data
  - Files: crates/core/src/historical/cross_verify.rs, crates/app/src/main.rs
  - Tests: test_cross_match_skips_when_live_mv_empty

- [x] 2. wait_with_backoff post-market guard — only in infinite-retry mode
  - Files: crates/core/src/websocket/connection.rs
  - Tests: 5 existing test_wait_with_backoff_* tests

- [x] 3. tick_processor broadcast — test-only received_at override
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: test_tick_processor_with_broadcast_channel

- [x] 4. notification strict — extract pure enforce_strict function
  - Files: crates/core/src/notification/service.rs
  - Tests: test_c1_initialize_strict_refuses_noop_and_respects_override
