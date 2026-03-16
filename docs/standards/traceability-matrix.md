# Requirements Traceability Matrix

> Maps every requirement to its tests. Every test to its requirement. No orphans.

## Format
REQ-XXX | Description | Tests | Status

---

## Core System Requirements

### REQ-001 | Parse ticker packet correctly per Dhan spec
Tests:
- `core::parser::ticker::tests::*` (unit)
- `core::tests::websocket_protocol_e2e::*` (integration)
- `core::tests::dhat_allocation::*` (performance)
Status: COVERED

### REQ-002 | Parse quote packet correctly per Dhan spec
Tests:
- `core::parser::quote::tests::*` (unit)
- `core::tests::websocket_protocol_e2e::*` (integration)
Status: COVERED

### REQ-003 | Parse full packet correctly per Dhan spec
Tests:
- `core::parser::full_packet::tests::*` (unit)
Status: COVERED

### REQ-004 | Parse OI data packet correctly per Dhan spec
Tests:
- `core::parser::oi::tests::*` (unit)
Status: COVERED

### REQ-005 | Parse previous close packet correctly per Dhan spec
Tests:
- `core::parser::previous_close::tests::*` (unit)
Status: COVERED

### REQ-006 | Parse market depth (5-level, 20-level, 200-level)
Tests:
- `core::parser::market_depth::tests::*` (unit)
- `core::parser::deep_depth::tests::*` (unit)
Status: COVERED

### REQ-007 | Response code dispatch routes to correct parser
Tests:
- `core::parser::dispatcher::tests::*` (unit)
- `core::parser::header::tests::*` (unit)
Status: COVERED

### REQ-008 | WebSocket connection with auth header
Tests:
- `core::websocket::connection::tests::*` (unit)
- `core::tests::gap_enforcement::ws_disconnect_codes::*` (integration)
Status: COVERED

### REQ-009 | Subscription batching (max 100 per message)
Tests:
- `core::websocket::subscription_builder::tests::*` (unit)
- `core::tests::gap_enforcement::ws_subscription_builder::*` (integration)
Status: COVERED

### REQ-010 | Connection pool (up to 5 connections)
Tests:
- `core::websocket::connection_pool::tests::*` (unit)
Status: COVERED

### REQ-011 | Keep-alive ping/pong (10s interval, 40s timeout)
Tests:
- `core::websocket::connection::tests::*` (unit)
Status: COVERED

### REQ-012 | Reconnection with exponential backoff
Tests:
- `core::websocket::connection::tests::*` (unit)
- `core::tests::gap_enforcement::ws_disconnect_codes::*` (integration)
Status: COVERED

### REQ-013 | Disconnect code handling (803/804 → token refresh)
Tests:
- `core::websocket::types::tests::*` (unit)
- `core::tests::gap_enforcement::ws_disconnect_codes::*` (integration)
Status: COVERED

---

## Authentication Requirements

### REQ-020 | AWS SSM secret retrieval
Tests:
- `core::auth::secret_manager::tests::*` (unit)
- `core::auth::secret_manager::proptest_ssm::*` (proptest)
Status: COVERED

### REQ-021 | TOTP code generation
Tests:
- `core::auth::totp_generator::tests::*` (unit)
Status: COVERED

### REQ-022 | Token renewal lifecycle (23h refresh, backoff)
Tests:
- `core::auth::token_manager::tests::*` (unit)
- `core::auth::types::tests::*` (unit)
- `core::tests::gap_enforcement::auth_token_lifecycle::*` (integration)
Status: COVERED

### REQ-023 | Token never stored in persistent state
Tests:
- `core::auth::types::tests::*` (security)
- `storage::tick_persistence::tests::test_ilp_row_does_not_contain_secret_or_token_data` (security)
Status: COVERED

---

## Instrument Management Requirements

### REQ-030 | CSV download with retry + fallback
Tests:
- `core::instrument::csv_downloader::tests::*` (unit)
- `core::instrument::instrument_loader::tests::*` (unit)
Status: COVERED

### REQ-031 | 5-pass mapping algorithm
Tests:
- `core::instrument::universe_builder::tests::*` (121 tests)
Status: COVERED

### REQ-032 | Duplicate security_id rejection (I-P0-01)
Tests:
- `core::instrument::universe_builder::tests::test_duplicate_security_id_rejected`
Status: COVERED

### REQ-033 | Daily CSV refresh scheduler (I-P1-01)
Tests:
- `core::instrument::daily_scheduler::tests::*` (unit)
Status: COVERED

### REQ-034 | Delta detection on instrument changes (I-P1-02)
Tests:
- `core::instrument::delta_detector::tests::*` (unit)
Status: COVERED

---

## Tick Processing Requirements

### REQ-040 | Tick deduplication (O(1) ring buffer)
Tests:
- `core::pipeline::tick_processor::tests::*` (unit)
Status: COVERED

### REQ-041 | QuestDB ILP tick writer (batched flush)
Tests:
- `storage::tick_persistence::tests::*` (unit)
- `storage::tests::dhat_tick_persistence::*` (performance)
Status: COVERED

### REQ-042 | f32→f64 precision (STORAGE-GAP-02)
Tests:
- `storage::tick_persistence::tests::test_f32_to_f64_clean_vs_naive_conversion`
- `storage::tick_persistence::tests::test_regression_f32_to_f64_precision_loss`
Status: COVERED

### REQ-043 | Tick DEDUP key includes segment (I-P1-06)
Tests:
- `storage::tick_persistence::tests::test_p1_06_dedup_key_includes_segment`
- `storage::tick_persistence::tests::test_regression_dedup_key_must_include_segment`
Status: COVERED

---

## Order Management Requirements

### REQ-050 | Order lifecycle state machine (26 valid transitions)
Tests:
- `trading::oms::state_machine::tests::*` (unit)
- `trading::tests::gap_enforcement::oms_state_machine::*` (integration)
Status: COVERED

### REQ-051 | Order reconciliation (pure function, mismatch detection)
Tests:
- `trading::oms::reconciliation::tests::*` (unit)
- `trading::tests::gap_enforcement::oms_reconciliation::*` (integration)
- `trading::tests::safety_layer::reconciliation_safety::*` (safety)
Status: COVERED

### REQ-052 | Circuit breaker (3-state FSM)
Tests:
- `trading::oms::circuit_breaker::tests::*` (unit)
- `trading::tests::gap_enforcement::oms_circuit_breaker::*` (integration)
- `trading::tests::loom_circuit_breaker::*` (concurrency)
Status: COVERED

### REQ-053 | SEBI rate limiting (10 orders/sec)
Tests:
- `trading::oms::rate_limiter::tests::*` (unit)
- `trading::tests::gap_enforcement::oms_rate_limiter::*` (integration)
Status: COVERED

### REQ-054 | Idempotency (correlation tracking)
Tests:
- `trading::oms::idempotency::tests::*` (unit)
- `trading::tests::gap_enforcement::oms_idempotency::*` (integration)
Status: COVERED

### REQ-055 | Dry-run safety gate (OMS-GAP-06)
Tests:
- `trading::oms::engine::tests::*` (unit)
Status: COVERED

---

## Risk Engine Requirements

### REQ-060 | Max daily loss enforcement
Tests:
- `trading::risk::engine::tests::daily_loss_breach_halts_trading` (unit)
- `trading::tests::gap_enforcement::risk_engine::*` (integration)
- `trading::tests::safety_layer::capital_protection::*` (safety)
- `trading::tests::financial_overflow::*` (overflow)
Status: COVERED

### REQ-061 | Position size limits
Tests:
- `trading::risk::engine::tests::check_order_rejected_position_limit` (unit)
- `trading::tests::safety_layer::capital_protection::test_position_size_at_exact_boundary` (safety)
Status: COVERED

### REQ-062 | Auto-halt on risk breach
Tests:
- `trading::risk::engine::tests::manual_halt_rejects_all_orders` (unit)
- `trading::tests::safety_layer::never_requirements::*` (NEVER)
Status: COVERED

### REQ-063 | Tick gap detection (RISK-GAP-03)
Tests:
- `trading::risk::tick_gap_tracker::tests::*` (unit)
- `trading::tests::gap_enforcement::risk_tick_gap::*` (integration)
Status: COVERED

---

## NEVER Requirements

### NEVER-018 | Capital limits must NEVER change at runtime
Tests:
- `trading::tests::safety_layer::never_requirements::test_never_018_limits_immutable_at_runtime`
- `trading::tests::safety_layer::never_requirements::test_never_018_limits_survive_daily_reset`
Status: COVERED

### NEVER-022 | Daily P&L counter must NEVER reset on reconnect
Tests:
- `trading::tests::safety_layer::never_requirements::test_never_022_pnl_counter_not_reset_by_halt_reset`
- `trading::tests::safety_layer::never_requirements::test_never_022_positions_survive_halt_reset`
- `trading::tests::safety_layer::never_requirements::test_never_022_only_reset_daily_clears_pnl`
Status: COVERED

---

## Alert & Notification Requirements

### REQ-070 | Critical events fire at Critical severity
Tests:
- `core::notification::events::tests::test_auth_failed_is_critical`
- `trading::tests::safety_layer::alert_routing::test_critical_events_are_critical_severity`
Status: COVERED

### REQ-071 | Alert messages include actionable context
Tests:
- `core::notification::events::tests::*` (all message format tests)
- `trading::tests::safety_layer::alert_routing::test_alert_messages_include_context`
Status: COVERED

### REQ-072 | Credentials never leak in alert messages
Tests:
- `core::notification::events::tests::test_auth_failed_redacts_credentials_in_url`
- `core::notification::events::tests::test_token_renewal_failed_redacts_credentials`
Status: COVERED

---

## Storage Requirements

### REQ-080 | QuestDB tick persistence (ILP protocol)
Tests:
- `storage::tick_persistence::tests::*` (109 tests)
- `storage::tests::dhat_tick_persistence::*` (DHAT)
Status: COVERED

### REQ-081 | Instrument persistence to QuestDB
Tests:
- `storage::instrument_persistence::tests::*` (89 tests)
Status: COVERED

### REQ-082 | ILP rows never contain secrets
Tests:
- `storage::tick_persistence::tests::test_ilp_row_does_not_contain_secret_or_token_data`
- `storage::tick_persistence::tests::test_redact_ilp_row_has_no_sensitive_fields`
Status: COVERED

---

## Summary

| Category | Total REQs | Covered | Gaps |
|----------|-----------|---------|------|
| Core Parser | 7 | 7 | 0 |
| Auth | 4 | 4 | 0 |
| Instruments | 5 | 5 | 0 |
| Tick Processing | 4 | 4 | 0 |
| OMS | 6 | 6 | 0 |
| Risk | 4 | 4 | 0 |
| NEVER | 2 | 2 | 0 |
| Alerts | 3 | 3 | 0 |
| Storage | 3 | 3 | 0 |
| **TOTAL** | **38** | **38** | **0** |
