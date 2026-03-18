# Implementation Plan: Deep Scan Fix — All Audit Findings

**Status:** VERIFIED
**Date:** 2026-03-17
**Approved by:** Parthiban

## Plan Items

- [x] 1. Add `PartTraded`, `Closed`, `Triggered` to `OrderStatus` enum
  - Files: crates/common/src/order_types.rs
  - Tests: test_order_status_as_str, test_order_status_serde_part_traded, test_order_status_serde_closed_and_triggered

- [x] 2. Update `parse_order_status()` to handle PART_TRADED, CLOSED, TRIGGERED
  - Files: crates/trading/src/oms/state_machine.rs
  - Tests: parse_all_known_statuses (updated with all 10+3 variants)

- [x] 3. Add PartTraded/Closed/Triggered transitions to state machine
  - Files: crates/trading/src/oms/state_machine.rs
  - Tests: pending_to_part_traded_valid, confirmed_to_part_traded_valid, part_traded_to_traded_valid, part_traded_to_cancelled_valid, part_traded_to_expired_valid, triggered_to_pending_valid, triggered_to_traded_valid, triggered_to_cancelled_valid, closed_to_anything_invalid

- [x] 4. Fix `correlationId` to ≤30 chars (Dhan limit)
  - Files: crates/trading/src/oms/idempotency.rs
  - Tests: generate_id_returns_30_char_hex_string, test_correlation_id_max_length_30, generate_id_returns_unique_ids

- [x] 5. Fix tick DEDUP key to include segment (STORAGE-GAP-01)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_tick_dedup_key_includes_segment

- [x] 6. Add MARKET order price=0 and SL triggerPrice>0 validation gates
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_market_order_nonzero_price_rejected, test_sl_order_zero_trigger_rejected, test_market_order_zero_price_accepted

- [x] 7. Add disclosedQuantity >30% validation gate
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_disclosed_quantity_below_30_percent_rejected

- [x] 8. Add `ExchangeSegment::from_byte()` reverse mapping
  - Files: crates/common/src/types.rs
  - Tests: test_from_byte_all_known_codes, test_from_byte_unknown_returns_none, test_from_byte_gap_at_6_returns_none, test_from_byte_roundtrips_with_binary_code

- [x] 9. Add `source` and `off_mkt_flag` fields to `OrderUpdate`
  - Files: crates/common/src/order_types.rs
  - Tests: test_order_update_serialize_roundtrip (updated)

- [x] 10. Add `modification_count` to `ManagedOrder` + enforcement
  - Files: crates/trading/src/oms/types.rs, crates/trading/src/oms/engine.rs
  - Tests: test_modification_count_enforced

- [x] 11. Update `is_terminal()` for new OrderStatus variants
  - Files: crates/trading/src/oms/types.rs
  - Tests: managed_order_terminal_states (PartTraded=non-terminal, Closed=terminal, Triggered=non-terminal)

- [x] 12. Update all affected test helpers and assertions
  - Files: crates/trading/src/oms/engine.rs, crates/trading/src/oms/reconciliation.rs, crates/trading/tests/gap_enforcement.rs, crates/trading/tests/safety_layer.rs, crates/storage/src/tick_persistence.rs

- [x] 13. Run full verification: cargo fmt + clippy + test
  - Result: 2,492 tests pass, 0 failures, clippy clean, fmt clean

## Scenarios

| # | Scenario | Expected | Verified |
|---|----------|----------|----------|
| 1 | Dhan WS sends "PART_TRADED" status | Parsed to OrderStatus::PartTraded | YES |
| 2 | UUID v4 generated for correlationId | Exactly 30 chars, valid charset | YES |
| 3 | Same security_id on NSE_EQ and BSE_EQ | Two distinct tick rows (DEDUP includes segment) | YES |
| 4 | MARKET order with price=100 submitted | OmsError::RiskRejected before API call | YES |
| 5 | SL order with triggerPrice=0 submitted | OmsError::RiskRejected before API call | YES |
| 6 | from_byte(6) called | Returns None (gap in Dhan's enum) | YES |
| 7 | Order modified 26th time | OmsError::RiskRejected (max 25) | YES |
