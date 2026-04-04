# Implementation Plan: Wire Missing Notification Events to Telegram

**Status:** DRAFT
**Date:** 2026-04-04
**Approved by:** pending

## Context

Monitoring agent found 7 notification events defined in events.rs but never fired
from production code. These events only go through ERROR log → Loki (2-min delay)
instead of immediate Telegram notification. Root cause: trading crate components
lack access to the notification service.

## Approach

Add an `mpsc::UnboundedSender<NotificationEvent>` to OMS engine and risk engine.
When events fire, send through channel. App crate wires receiver to notifier.
Cold path only (orders are 1-100/day) — unbounded channel acceptable.

## Plan Items

- [ ] Item 1: Add notification sender to OrderManagementSystem
  - Files: engine.rs
  - Tests: test_oms_fires_circuit_breaker_opened_event, test_oms_fires_order_rejected_event, test_oms_fires_rate_limit_event

- [ ] Item 2: Fire CircuitBreakerOpened/Closed from circuit_breaker.rs via callback
  - Files: circuit_breaker.rs, engine.rs
  - Tests: test_circuit_breaker_fires_opened_on_threshold, test_circuit_breaker_fires_closed_on_recovery

- [ ] Item 3: Fire RateLimitExhausted from rate_limiter check in engine
  - Files: engine.rs
  - Tests: test_rate_limit_fires_notification

- [ ] Item 4: Fire OrderRejected from engine place_order error paths
  - Files: engine.rs
  - Tests: test_order_rejected_fires_notification

- [ ] Item 5: Fire RiskHalt from risk engine halt path
  - Files: engine.rs
  - Tests: test_risk_halt_fires_notification

- [ ] Item 6: Fire WebSocketReconnectionExhausted from connection pool
  - Files: connection.rs
  - Tests: test_ws_reconnect_exhausted_fires_notification

- [ ] Item 7: Fire QuestDbReconnected from tick persistence drain
  - Files: main.rs
  - Tests: test_questdb_reconnected_fires_notification

- [ ] Item 8: Wire notification channel in main.rs boot sequence
  - Files: main.rs
  - Tests: existing boot tests

- [ ] Run full build verification (fmt + clippy + test)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Circuit breaker opens | Immediate Telegram: "Circuit breaker OPEN" |
| 2 | Circuit breaker closes | Immediate Telegram: "Circuit breaker CLOSED" |
| 3 | Rate limit hit | Immediate Telegram: "Rate limit exhausted" |
| 4 | Order rejected | Immediate Telegram: "Order REJECTED" |
| 5 | Risk halt | Immediate Telegram: "Risk HALT" |
| 6 | WS reconnect exhausted | Immediate Telegram: "WS reconnection exhausted" |
| 7 | QuestDB recovers | Immediate Telegram: "QuestDB reconnected" |
