# Implementation Plan: Comprehensive Testing & API Validation Gaps

**Status:** APPROVED
**Date:** 2026-04-02
**Approved by:** Parthiban ("Go ahead dude")

## Context

Continuing from previous session's 15-item plan that couldn't execute due to shell issues.
QuestDB resilience (7 items) is done. These are the remaining gaps from that plan:
API reference accuracy fixes + test reinforcement for candle persistence.

## Plan Items

- [ ] P1: Add super order price validation (target/SL relative to entry price)
  - Files: crates/trading/src/oms/engine.rs, crates/trading/src/oms/types.rs
  - Tests: test_super_order_buy_target_must_exceed_entry, test_super_order_buy_sl_must_be_below_entry,
           test_super_order_sell_target_must_be_below_entry, test_super_order_sell_sl_must_exceed_entry,
           test_super_order_valid_buy_prices_accepted, test_super_order_valid_sell_prices_accepted

- [ ] P2: Add ExpiryCode enum with validation (replace raw u8)
  - Files: crates/common/src/instrument_types.rs, crates/core/src/historical/candle_fetcher.rs
  - Tests: test_expiry_code_serialize_to_u8, test_expiry_code_all_variants,
           test_expiry_code_from_u8_invalid_rejected

- [ ] P3: Add LiveCandleWriter cold-start tests (QuestDB unavailable at startup)
  - Files: crates/storage/src/candle_persistence.rs (add new_disconnected factory),
           crates/storage/tests/candle_resilience.rs (new test file)
  - Tests: test_candle_writer_cold_start_succeeds, test_candle_writer_cold_start_buffers_candles,
           test_candle_writer_cold_start_zero_drops

- [ ] P4: Add recovery ordering integration test (ring buffer drains before disk spill)
  - Files: crates/storage/tests/candle_resilience.rs
  - Tests: test_recovery_ordering_ring_before_spill

- [ ] P5: Add schema validation after QuestDB reconnect + test
  - Files: crates/storage/src/candle_persistence.rs (reconnect method),
           crates/storage/tests/candle_resilience.rs
  - Tests: test_reconnect_revalidates_schema

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Super order BUY with target < entry | Rejected before API call |
| 2 | Super order SELL with SL < entry | Rejected before API call |
| 3 | QuestDB down at candle writer startup | Writer starts disconnected, candles buffered |
| 4 | Ring buffer + spill during outage → reconnect | Ring drains first, then spill |
| 5 | QuestDB restarts mid-session | Reconnect re-runs DDL validation |
