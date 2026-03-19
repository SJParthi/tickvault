# Implementation Plan: Fix All Infrastructure, Observability & Automation Gaps

**Status:** DRAFT
**Date:** 2026-03-19
**Approved by:** pending

## Context

Eagle's Eye Audit found 22 gaps across Docker, observability, notifications, health checks, metrics, and storage. This plan fixes ALL of them in priority order. Grouped into 6 batches for reviewability.

---

## Batch 1: Storage DEDUP Bug Fix (P0 — data correctness)

- [ ] Fix `previous_close` DEDUP key to include `segment`
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_previous_close_dedup_key_includes_segment, test_market_depth_dedup_key_includes_segment

- [ ] Fix `market_depth` DEDUP key to include `segment`
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_market_depth_dedup_key_includes_segment

## Batch 2: Rich Health Endpoint (P0 — system readiness)

- [ ] Add `SystemHealth` struct with subsystem status (QuestDB, WebSocket, token, pipeline)
  - Files: crates/api/src/handlers/health.rs, crates/api/src/state.rs
  - Tests: test_health_check_returns_subsystem_status, test_health_degraded_when_ws_disconnected

- [ ] Wire health endpoint to check QuestDB reachability, WS connection count, token validity, pipeline active status
  - Files: crates/api/src/handlers/health.rs, crates/api/src/state.rs, crates/app/src/main.rs
  - Tests: test_health_check_degraded, test_health_check_healthy

## Batch 3: Notification Wiring (P0 — alerting)

- [ ] Fire `BootHealthCheck` event after Docker services verified healthy
  - Files: crates/app/src/main.rs
  - Tests: existing boot tests still pass

- [ ] Fire `BootDeadlineMissed` event when boot step exceeds deadline
  - Files: crates/app/src/main.rs
  - Tests: existing boot tests still pass

- [ ] Add OMS notification events: `OrderRejected`, `CircuitBreakerOpened`, `RateLimitExhausted`
  - Files: crates/core/src/notification/events.rs
  - Tests: test_oms_event_severity, test_oms_event_formatting

- [ ] Wire circuit breaker state changes → notification service
  - Files: crates/trading/src/oms/circuit_breaker.rs, crates/core/src/notification/events.rs
  - Tests: test_circuit_breaker_notify_on_open

- [ ] Wire risk engine violations → notification service (daily loss breach, auto-halt)
  - Files: crates/trading/src/risk/engine.rs, crates/core/src/notification/events.rs
  - Tests: test_risk_halt_notification

- [ ] Wire order update WS disconnect → notification
  - Files: crates/core/src/websocket/order_update_connection.rs
  - Tests: existing order update WS tests pass

## Batch 4: Metrics Gaps (P1 — observability)

- [ ] Add OMS placeholder metrics: `dlt_orders_placed_total`, `dlt_orders_rejected_total`, `dlt_orders_filled_total`
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_oms_metrics_emitted_on_place, test_oms_metrics_emitted_on_reject

- [ ] Add `dlt_wire_to_done_duration_ns` histogram in tick processor
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: existing tick processor tests pass

- [ ] Add `#[instrument]` spans on key functions: token renewal, WS connect, order placement, risk check
  - Files: crates/core/src/auth/token_manager.rs, crates/core/src/websocket/connection.rs, crates/trading/src/oms/engine.rs, crates/trading/src/risk/engine.rs
  - Tests: existing tests pass (tracing spans are passive)

## Batch 5: Docker Hardening (P1 — production readiness)

- [ ] Add `mem_limit` + `cpus` resource limits to all Docker services
  - Files: deploy/docker/docker-compose.yml
  - Tests: manual docker compose config validation

- [ ] Add `logging` driver config (`json-file`, max-size 100m, max-file 5) to all services
  - Files: deploy/docker/docker-compose.yml
  - Tests: manual docker compose config validation

- [ ] Fix Loki `depends_on` in Alloy: use `service_healthy` (Loki now has working healthcheck)
  - Files: deploy/docker/docker-compose.yml
  - Tests: manual docker compose config validation

- [ ] Add Traefik basicAuth middleware for Grafana, QuestDB, Jaeger in dev mode
  - Files: deploy/docker/traefik/dynamic/routers.yml
  - Tests: manual verification

- [ ] Add Valkey metrics scrape job to Prometheus config
  - Files: deploy/docker/prometheus/prometheus.yml
  - Tests: manual verification

## Batch 6: Robustness Improvements (P2)

- [ ] Add `segment` to previous_close and market_depth `build_*_rows` functions
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_previous_close_row_includes_segment

- [ ] Add rate-limit error exclusion from circuit breaker failure counter
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_rate_limit_error_does_not_trip_circuit_breaker

- [ ] Add WebSocket reconnection global timeout with CRITICAL notification
  - Files: crates/core/src/websocket/connection.rs
  - Tests: test_ws_reconnection_global_timeout

- [ ] Add token renewal absolute deadline (market-hours-based: if renewal fails past 14:00 IST → CRITICAL)
  - Files: crates/core/src/auth/token_manager.rs
  - Tests: test_token_renewal_absolute_deadline

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Health check with all services up | status: "healthy", all subsystems green |
| 2 | Health check with WS down | status: "degraded", websocket: "disconnected" |
| 3 | Circuit breaker opens | Telegram HIGH alert sent |
| 4 | Daily loss breached | Telegram HIGH alert, auto-halt activated |
| 5 | Docker Jaeger hits 2GB mem limit | Container restarts (restart: unless-stopped) |
| 6 | Previous close for same security_id, different segment | Both stored (dedup key includes segment) |
| 7 | Rate limit error in OMS | Circuit breaker NOT incremented |
| 8 | WS reconnection exhausted | CRITICAL notification sent |
