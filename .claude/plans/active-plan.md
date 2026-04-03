# Implementation Plan: Comprehensive Testing & API Validation Gaps

**Status:** VERIFIED
**Date:** 2026-04-02
**Approved by:** Parthiban ("Go ahead dude")

## Context

Continuing from previous session's 15-item plan that couldn't execute due to shell issues.
QuestDB resilience (7 items) is done. These are the remaining gaps from that plan:
API reference accuracy fixes + test reinforcement for candle persistence.

## Plan Items

- [x] P1: Add super order price validation (target/SL relative to entry price)
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_super_order_buy_target_must_exceed_entry, test_super_order_buy_sl_must_be_below_entry,
           test_super_order_sell_target_must_be_below_entry, test_super_order_sell_sl_must_exceed_entry,
           test_super_order_valid_buy_prices_accepted, test_super_order_valid_sell_prices_accepted,
           test_super_order_zero_entry_price_rejected, test_super_order_zero_target_price_rejected,
           test_super_order_zero_sl_price_rejected, test_super_order_buy_target_equal_entry_rejected,
           test_super_order_buy_sl_equal_entry_rejected, test_super_order_unknown_txn_type_rejected
  - Impl: validate_super_order_prices() at engine.rs:718

- [x] P2: Add ExpiryCode enum with validation (replace raw u8)
  - Files: crates/common/src/instrument_types.rs, crates/core/src/historical/candle_fetcher.rs
  - Tests: test_expiry_code_serialize_to_u8, test_expiry_code_all_variants,
           test_expiry_code_from_u8_invalid_rejected, test_expiry_code_roundtrip,
           test_expiry_code_display
  - Impl: ExpiryCode enum at instrument_types.rs:117, DailyRequest updated to use ExpiryCode

- [x] P3: Add LiveCandleWriter cold-start tests (QuestDB unavailable at startup)
  - Files: crates/storage/tests/candle_resilience.rs (new test file)
  - Tests: test_candle_writer_cold_start_succeeds, test_candle_writer_cold_start_buffers_candles,
           test_candle_writer_cold_start_zero_drops

- [x] P4: Add recovery ordering integration test (ring buffer drains before disk spill)
  - Files: crates/storage/tests/candle_resilience.rs
  - Tests: test_recovery_ordering_ring_before_spill

- [x] P5: Add schema validation after QuestDB reconnect + test
  - Files: crates/storage/tests/candle_resilience.rs
  - Tests: test_reconnect_revalidates_schema, test_disconnected_writer_fresh_buffer_does_not_corrupt

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Super order BUY with target < entry | Rejected before API call |
| 2 | Super order SELL with SL < entry | Rejected before API call |
| 3 | QuestDB down at candle writer startup | Writer starts disconnected, candles buffered |
| 4 | Ring buffer + spill during outage → reconnect | Ring drains first, then spill |
| 5 | QuestDB restarts mid-session | Reconnect re-runs DDL validation |
