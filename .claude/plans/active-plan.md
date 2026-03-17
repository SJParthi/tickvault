# Implementation Plan: Deep Scan Fix — All Audit Findings

**Status:** DRAFT
**Date:** 2026-03-17
**Approved by:** pending

## Plan Items

- [ ] 1. Add `PartTraded`, `Closed`, `Triggered` to `OrderStatus` enum
  - Files: crates/common/src/order_types.rs
  - Tests: test_order_status_as_str, test_order_status_serde_roundtrip (updated for new variants)
  - Details: Dhan sends PART_TRADED, CLOSED, TRIGGERED. Missing these causes silent order update drops.

- [ ] 2. Update `parse_order_status()` to handle PART_TRADED, CLOSED, TRIGGERED
  - Files: crates/trading/src/oms/state_machine.rs
  - Tests: parse_all_known_statuses (updated), test_parse_part_traded, test_parse_closed, test_parse_triggered
  - Details: Add "PART_TRADED" | "PARTIALLY_FILLED" → PartTraded, "CLOSED" → Closed, "TRIGGERED" → Triggered

- [ ] 3. Add PartTraded/Closed transitions to state machine
  - Files: crates/trading/src/oms/state_machine.rs
  - Tests: test_pending_to_part_traded_valid, test_confirmed_to_part_traded_valid, test_part_traded_to_traded_valid, test_part_traded_to_cancelled_valid, test_closed_terminal
  - Details: Pending/Confirmed → PartTraded, PartTraded → Traded/Cancelled/Expired

- [ ] 4. Fix `correlationId` to ≤30 chars (Dhan limit)
  - Files: crates/trading/src/oms/idempotency.rs
  - Tests: test_correlation_id_max_length_30, generate_id_returns_unique_ids (still passes)
  - Details: UUID v4 simple format = 32 hex chars. Truncate to 30. Dhan allows `[a-zA-Z0-9 _-]`.

- [ ] 5. Fix tick DEDUP key to include segment (STORAGE-GAP-01)
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: test_tick_dedup_key_includes_segment (update existing assertions)
  - Details: Change `DEDUP_KEY_TICKS = "security_id"` → `"security_id, segment"`. Same security_id can exist on NSE_EQ and BSE_EQ.

- [ ] 6. Add MARKET order price=0 and SL triggerPrice>0 validation gates
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_market_order_nonzero_price_rejected, test_sl_order_zero_trigger_rejected
  - Details: Reject before Dhan API call to avoid wasting rate limit on DH-905.

- [ ] 7. Add disclosedQuantity >30% validation gate
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_disclosed_quantity_below_30_percent_rejected
  - Details: Dhan rejects if disclosed_quantity > 0 && disclosed_quantity < 30% of quantity.

- [ ] 8. Add `ExchangeSegment::from_byte()` reverse mapping
  - Files: crates/common/src/types.rs
  - Tests: test_from_byte_all_known_codes, test_from_byte_unknown_returns_none, test_from_byte_gap_at_6_returns_none, test_from_byte_roundtrips_with_binary_code
  - Details: Byte → enum for binary packet parsing. Returns None for unknown (including gap at 6).

- [ ] 9. Add `source` and `off_mkt_flag` fields to `OrderUpdate`
  - Files: crates/common/src/order_types.rs
  - Tests: test_order_update_deserialize_full_message (updated), test_order_update_source_field
  - Details: `source` = "P" for API orders. `off_mkt_flag` = "1" for AMO. Both needed for filtering.

- [ ] 10. Add `modification_count` to `ManagedOrder` + enforcement
  - Files: crates/trading/src/oms/types.rs, crates/trading/src/oms/engine.rs
  - Tests: test_modification_count_enforced
  - Details: Track per-order modification count. Reject at 25 (Dhan max).

- [ ] 11. Update `is_terminal()` for new OrderStatus variants
  - Files: crates/trading/src/oms/types.rs
  - Tests: managed_order_terminal_states (updated for PartTraded=non-terminal, Closed=terminal, Triggered=non-terminal)

- [ ] 12. Update all affected test helpers and assertions
  - Files: crates/trading/src/oms/engine.rs (make_order_update), crates/storage/src/tick_persistence.rs (dedup tests)
  - Tests: All existing tests must still compile and pass

- [ ] 13. Run full verification: cargo fmt + clippy + test
  - Files: workspace-wide
  - Tests: `cargo test --workspace`

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan WS sends "PART_TRADED" status | Parsed to OrderStatus::PartTraded, valid transition from Pending/Confirmed |
| 2 | UUID v4 generated for correlationId | Exactly 30 chars, valid charset [a-zA-Z0-9] |
| 3 | Same security_id on NSE_EQ and BSE_EQ | Two distinct tick rows in QuestDB (not overwritten) |
| 4 | MARKET order with price=100 submitted | OmsError::RiskRejected before API call |
| 5 | SL order with triggerPrice=0 submitted | OmsError::RiskRejected before API call |
| 6 | from_byte(6) called | Returns None (gap in Dhan's enum) |
| 7 | Order modified 26th time | OmsError::RiskRejected (max 25 modifications) |
