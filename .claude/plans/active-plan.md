# Implementation Plan: Wire Missing Notification Events to Telegram

**Status:** VERIFIED
**Date:** 2026-04-04
**Approved by:** Parthiban (2026-04-04)

## Context

Monitoring agent found 7 notification events defined in events.rs but never fired
from production code. These events only go through ERROR log → Loki (2-min delay)
instead of immediate Telegram notification. Root cause: trading crate components
lack access to the notification service (core crate not in trading deps).

## Approach

Used callback traits (OmsAlertSink, RiskAlertSink) in trading crate to decouple
from core notification crate. App crate implements the traits bridging to
NotificationService. QuestDB reconnected fires directly from app crate.

## Plan Items

- [x] Item 1: Add OmsAlertSink trait and alert_sink field to OMS engine
  - Files: engine.rs
  - Impl: OmsAlert enum + OmsAlertSink trait + set_alert_sink() + fire_alert()

- [x] Item 2: Fire CircuitBreakerOpened/Closed via OmsAlertSink
  - Files: circuit_breaker.rs, engine.rs
  - Impl: failure_count() + was_previously_open() on circuit breaker, fire_alert in place_order

- [x] Item 3: Fire RateLimitExhausted from engine place_order
  - Files: engine.rs
  - Impl: fire_alert(OmsAlert::RateLimitExhausted) on rate_limiter.check() failure

- [x] Item 4: Fire OrderRejected from engine place_order API error
  - Files: engine.rs
  - Impl: fire_alert(OmsAlert::OrderRejected) on Dhan API error

- [x] Item 5: Fire RiskHalt from risk engine trigger_halt
  - Files: engine.rs (risk)
  - Impl: RiskAlertSink trait + fire_risk_halt() in trigger_halt()

- [x] Item 6: WebSocketReconnectionExhausted — deferred (infinite retry mode)
  - Files: connection.rs
  - Note: WS uses reconnect_max_attempts=0 (infinite retries). Event fires ERROR every 10 failures already.

- [x] Item 7: Fire QuestDbReconnected from tick persistence consumer
  - Files: main.rs
  - Impl: Added notifier param to run_tick_persistence_consumer, fires on take_recovery_flag()

- [x] Item 8: Verified compilation + tests
  - cargo fmt: clean
  - cargo clippy: clean
  - cargo test: 1,317 passed, 0 failed
