# Implementation Plan: Fully Automated Infrastructure Self-Healing

**Status:** VERIFIED
**Date:** 2026-04-06
**Approved by:** Parthiban

## Plan Items

- [x] Add `restart: unless-stopped` to remaining Docker services missing it
  - Files: deploy/docker/docker-compose.yml (already had it on all 9 services)
  - Tests: Verified via grep — all services have restart: unless-stopped

- [x] Harden `ensure_infra_running()` with retry loop for `docker compose up -d`
  - Files: crates/app/src/infra.rs (lines 157-188)
  - Tests: test_compose_retry_constants_are_reasonable

- [x] Add `check_and_restart_containers()` — watchdog function that monitors containers and auto-restarts
  - Files: crates/app/src/infra.rs (lines 503-600)
  - Tests: test_watchdog_interval_is_reasonable, test_check_and_restart_containers_no_panic

- [x] Wire watchdog into periodic health check loop (existing 5-min loop in main.rs)
  - Files: crates/app/src/main.rs (C5 block after C4)
  - Tests: existing health check tests cover wiring

- [x] Deferred browser auto-open after containers confirmed healthy
  - Files: crates/app/src/infra.rs (already deferred — opens after wait_for_service_healthy)
  - Tests: test_grafana_url_is_valid (existing)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Docker Desktop not running at boot | App launches Docker Desktop, retries compose up 5x, waits for health |
| 2 | Docker running but containers not started | compose up -d starts containers with retry, waits for health |
| 3 | Container crashes mid-session | Watchdog detects within 5min, auto-restarts, Telegram alert |
| 4 | QuestDB down for extended period | Ticks spill to disk, watchdog retries compose, auto-drain on recovery |
| 5 | All containers healthy | Watchdog is no-op, no alerts |
