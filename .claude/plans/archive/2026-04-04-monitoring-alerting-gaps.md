# Implementation Plan: Monitoring & Alerting Gaps Fix

**Status:** VERIFIED
**Date:** 2026-04-04
**Approved by:** Parthiban (2026-04-04)

## Context

Deep research via 8 parallel agents verified the entire codebase against Dhan docs + Python SDK.
All binary parsers, enums, protocols, 31 gap enforcement tests, QuestDB resilience, and sandbox
enforcement are confirmed correct. Five monitoring/alerting gaps were identified.

## Plan Items

- [x] Item 1: Wire Prometheus Alertmanager → Telegram notification
  - Files: docker-compose.yml, alertmanager.yml, prometheus.yml
  - Impl: Added Alertmanager Docker service, webhook config, Prometheus alerting section

- [x] Item 2: QuestDB liveness check → Telegram alert on failure
  - Files: main.rs
  - Impl: Health check loop now fires QuestDbDisconnected notification when liveness ping fails

- [x] Item 3: Add spill file auto-cleanup with 7-day TTL after drain
  - Files: infra.rs, constants.rs
  - Impl: cleanup_old_spill_files removes files older than SPILL_FILE_MAX_AGE_SECS, called in health loop

- [x] Item 4: Circuit breaker alert rule via Prometheus → Alertmanager → Telegram
  - Files: dlt-alerts.yml
  - Impl: Added CircuitBreakerOpen alert rule

- [x] Item 5: Circuit breaker OPEN → halt new orders (ALREADY IMPLEMENTED)
  - Files: engine.rs
  - Impl: Already returns Err CircuitBreakerOpen when circuit is open

- [x] Run full build verification (fmt + clippy + test)
  - cargo fmt --check: clean
  - cargo clippy --workspace -- -D warnings: clean
  - cargo test: 1,317 passed, 0 failed

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | QuestDB down for 30+ min | Prometheus alert fires → Alertmanager → Telegram notification |
| 2 | QuestDB liveness check fails | Metric updated, notification triggered |
| 3 | Spill files older than 7 days after drain | Auto-deleted |
| 4 | Spill files from today | Preserved |
| 5 | Circuit breaker opens | New orders rejected with OmsError |
| 6 | Circuit breaker closes (half-open probe succeeds) | Orders allowed again |
| 7 | Critical error in boot/pipeline | Telegram alert sent automatically |
